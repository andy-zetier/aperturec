use aperturec_graphics::prelude::*;
use aperturec_protocol::common::Codec;
use aperturec_protocol::media::{server_to_client as mm_s2c, EmptyFrameTerminal, FrameFragment};

use anyhow::{anyhow, bail, ensure, Result};
use flate2::read::DeflateDecoder;
use ndarray::prelude::*;
use range_set_blaze::RangeSetBlaze;
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::io::Read;
use std::mem;
use std::sync::{LazyLock, Mutex};
use tracing::*;

fn decode_raw(encoded_data: Vec<u8>) -> Result<Vec<u8>> {
    Ok(encoded_data)
}

fn decode_zlib(encoded_data: Vec<u8>) -> Result<Vec<u8>> {
    let mut z = DeflateDecoder::new(&*encoded_data);
    let mut decoded = vec![];
    z.read_to_end(&mut decoded)?;
    Ok(decoded)
}

struct JxlDecoder<'a, 'b>(jpegxl_rs::decode::JxlDecoder<'a, 'b>);

// SAFETY: jpegxl_rs::decode::JxlDecoder is only !Send because it has some raw pointers in it.
// However, we know that these raw pointers will be created in global memory (`LazyLock`), and only
// ever accessed from one thread at a time `ResourcePool`. So we can use a new-type here and mark
// the new-type as Send
unsafe impl Send for JxlDecoder<'_, '_> {}

fn decode_jpegxl(encoded_data: Vec<u8>) -> Result<Vec<u8>> {
    static DECODER: LazyLock<Mutex<JxlDecoder>> = LazyLock::new(|| {
        let par = jpegxl_rs::ThreadsRunner::default();
        Mutex::new(JxlDecoder(
            jpegxl_rs::decoder_builder()
                .parallel_runner(Box::leak(Box::new(par)))
                .build()
                .expect("decoder build"),
        ))
    });
    let pixels = DECODER
        .lock()
        .expect("JxlDecoder")
        .0
        .decode(&encoded_data)?
        .1;
    match pixels {
        jpegxl_rs::decode::Pixels::Uint8(vec) => Ok(vec),
        _ => bail!("unsupppored pixel format"),
    }
}

fn decode(codec: Codec, encoded_data: Vec<u8>) -> Result<Vec<u8>> {
    match codec {
        Codec::Raw => decode_raw(encoded_data),
        Codec::Zlib => decode_zlib(encoded_data),
        Codec::Jpegxl => decode_jpegxl(encoded_data),
        codec => bail!("Unsupported codec: {:?}", codec),
    }
}

#[derive(Debug, Clone)]
struct EncodedFragmentData {
    frame: usize,
    decoder: usize,
    sequence: usize,
    terminal: bool,
    codec: Codec,
    size: Size,
    decoder_relative_origin: Point,
    encoded_pixels: Vec<u8>,
}

impl TryFrom<FrameFragment> for EncodedFragmentData {
    type Error = anyhow::Error;

    fn try_from(frag: FrameFragment) -> Result<Self> {
        let loc = frag.location.ok_or(anyhow!("no origin"))?;
        let dim = frag.dimension.ok_or(anyhow!("no dimension"))?;
        let origin = Point::new(loc.x_position.try_into()?, loc.y_position.try_into()?);
        let size = Size::new(dim.width.try_into()?, dim.height.try_into()?);

        Ok(EncodedFragmentData {
            frame: frag.frame.try_into()?,
            decoder: frag.encoder.try_into()?,
            sequence: frag.sequence.try_into()?,
            terminal: frag.terminal,
            codec: frag.codec.try_into()?,
            size,
            decoder_relative_origin: origin,
            encoded_pixels: frag.data,
        })
    }
}

impl EncodedFragmentData {
    fn decode(self) -> Result<DecodedFragmentData> {
        let decoded_data = decode(self.codec, self.encoded_pixels)?;
        ensure!(
            decoded_data.len() == self.size.area() * mem::size_of::<Pixel24>(),
            "Decompressed Length {} < {}x{}x{} = {}",
            decoded_data.len(),
            self.size.width,
            self.size.height,
            mem::size_of::<Pixel24>(),
            self.size.area() * mem::size_of::<Pixel24>(),
        );

        // SAFETY: We compute `len` and `capacity` to ensure that the `pixels24_vec` is the correct
        // size. We create a `Vec` with the pointer to the decoded data, but `mem::forget` that
        // data first. The call to `forget` effectively ensures that the `pixels24_vec` will get
        // ownership of the data and that when `decoded_data` goes out of scope, the underlying
        // data is not freed.
        let pixels24 = unsafe {
            let len = decoded_data.len() / mem::size_of::<Pixel24>();
            let capacity = decoded_data.capacity() / mem::size_of::<Pixel24>();
            let ptr = decoded_data.as_ptr();

            mem::forget(decoded_data);
            let pixels24_vec = Vec::from_raw_parts(ptr as *mut Pixel24, len, capacity);
            Array2::from_shape_vec(self.size.as_shape(), pixels24_vec)?
        };

        Ok(DecodedFragmentData {
            frame: self.frame,
            decoder: self.decoder,
            sequence: self.sequence,
            terminal: self.terminal,
            decoder_relative_origin: self.decoder_relative_origin,
            pixmap: pixels24,
        })
    }
}

#[derive(Debug, Clone)]
struct DecodedFragmentData {
    frame: usize,
    decoder: usize,
    sequence: usize,
    terminal: bool,
    decoder_relative_origin: Point,
    pixmap: Pixel24Map,
}

#[derive(Debug, Clone)]
enum DecoderFrameState {
    InFlight {
        received: RangeSetBlaze<usize>,
    },
    TerminatedIncomplete {
        missing_sequences: RangeSetBlaze<usize>,
    },
    Complete,
}

impl Default for DecoderFrameState {
    fn default() -> Self {
        DecoderFrameState::InFlight {
            received: RangeSetBlaze::new(),
        }
    }
}

impl DecoderFrameState {
    fn mark_sequence_received(&mut self, sequence: usize, terminal: bool) -> Result<()> {
        match self {
            DecoderFrameState::InFlight { received } => {
                ensure!(
                    !received.contains(sequence),
                    "identical sequence {}",
                    sequence
                );

                received.insert(sequence);

                if terminal {
                    if received.first() == Some(0) && received.ranges_len() == 1 {
                        *self = DecoderFrameState::Complete
                    } else {
                        let missing_sequences =
                            RangeSetBlaze::from_iter([0..=sequence]) - &*received;

                        *self = DecoderFrameState::TerminatedIncomplete { missing_sequences }
                    }
                }
            }
            DecoderFrameState::TerminatedIncomplete { missing_sequences } => {
                ensure!(!terminal, "received duplicate terminals");

                missing_sequences.remove(sequence);
                if missing_sequences.is_empty() {
                    *self = DecoderFrameState::Complete
                }
            }
            DecoderFrameState::Complete { .. } => {
                bail!("marking sequence received for complete decoder frame")
            }
        }
        Ok(())
    }

    fn mark_empty_frame_terminal_received(&mut self) -> Result<()> {
        match self {
            DecoderFrameState::InFlight { received } => {
                ensure!(
                    received.is_empty(),
                    "marking non-empty frame terminated with empty frame terminal"
                );
                *self = DecoderFrameState::Complete;
                Ok(())
            }
            _ => bail!("marking empty frame terminal received for previously terminated frame"),
        }
    }
}

#[derive(Debug)]
struct InFlightFrame {
    decoder_frame_states: Box<[DecoderFrameState]>,
    decoded_fragments: Vec<DecodedFragmentData>,
}

impl InFlightFrame {
    fn new(areas: &[Box2D]) -> Self {
        InFlightFrame {
            decoder_frame_states: vec![DecoderFrameState::default(); areas.len()]
                .into_boxed_slice(),
            decoded_fragments: vec![],
        }
    }

    fn add_fragment(&mut self, frag: DecodedFragmentData) -> Result<()> {
        ensure!(
            frag.decoder < self.decoder_frame_states.len(),
            "Invalid decoder {}",
            frag.decoder
        );
        self.decoder_frame_states[frag.decoder]
            .mark_sequence_received(frag.sequence, frag.terminal)?;
        self.decoded_fragments.push(frag);
        Ok(())
    }

    fn try_complete(self) -> Result<CompleteFrame, Self> {
        if self
            .decoder_frame_states
            .iter()
            .all(|s| matches!(s, DecoderFrameState::Complete))
        {
            Ok(CompleteFrame {
                decoded_fragments: self.decoded_fragments,
            })
        } else {
            Err(self)
        }
    }

    fn force_complete(self) -> Result<CompleteFrame, (EarlyDrawFrame, Vec<DecodedFragmentData>)> {
        if self
            .decoder_frame_states
            .iter()
            .all(|s| matches!(s, DecoderFrameState::Complete))
        {
            Ok(CompleteFrame {
                decoded_fragments: self.decoded_fragments,
            })
        } else {
            Err((
                EarlyDrawFrame {
                    decoder_frame_states: self.decoder_frame_states,
                },
                self.decoded_fragments,
            ))
        }
    }

    fn terminate_empty(&mut self, decoder: usize) -> Result<()> {
        ensure!(
            decoder < self.decoder_frame_states.len(),
            "Invalid decoder {}",
            decoder
        );
        self.decoder_frame_states[decoder].mark_empty_frame_terminal_received()?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct EarlyDrawFrame {
    decoder_frame_states: Box<[DecoderFrameState]>,
}

impl EarlyDrawFrame {
    fn mark_fragment_received(
        &mut self,
        decoder: usize,
        sequence: usize,
        terminal: bool,
    ) -> Result<()> {
        ensure!(
            decoder < self.decoder_frame_states.len(),
            "mark fragment {} received for decoder {}, which does not exist",
            sequence,
            decoder
        );

        self.decoder_frame_states[decoder].mark_sequence_received(sequence, terminal)?;
        Ok(())
    }

    fn mark_empty_frame_terminal_received(&mut self, decoder: usize) -> Result<()> {
        ensure!(
            decoder < self.decoder_frame_states.len(),
            "mark empty frame terminal received for decoder {}, which does not exist",
            decoder
        );

        self.decoder_frame_states[decoder].mark_empty_frame_terminal_received()?;
        Ok(())
    }

    fn try_complete(self) -> Result<(), Self> {
        if self
            .decoder_frame_states
            .iter()
            .all(|s| matches!(s, DecoderFrameState::Complete))
        {
            Ok(())
        } else {
            Err(self)
        }
    }
}

#[derive(Debug)]
pub struct CompleteFrame {
    decoded_fragments: Vec<DecodedFragmentData>,
}

#[derive(Debug)]
pub struct Draw {
    pub frame: usize,
    pub origin: Point,
    pub pixels: Pixel24Map,
}

impl Draw {
    pub fn area(&self) -> Box2D {
        Rect::new(self.origin, self.pixels.size()).to_box2d()
    }
}

#[derive(Default)]
struct DrawState {
    undrawn_fragments: Vec<DecodedFragmentData>,
}

impl DrawState {
    fn queue(&mut self, frag: DecodedFragmentData) {
        self.undrawn_fragments.push(frag);
    }

    fn is_empty(&self) -> bool {
        self.undrawn_fragments.is_empty()
    }
}

pub struct Framer {
    areas: Box<[Box2D]>,
    in_flight: BTreeMap<usize, InFlightFrame>,
    early_draw: BTreeMap<usize, EarlyDrawFrame>,
    complete: RangeSetBlaze<usize>,
    draw_state: DrawState,
}

impl Framer {
    pub fn new(areas: &[Box2D]) -> Self {
        Framer {
            areas: areas.to_vec().into_boxed_slice(),
            in_flight: BTreeMap::new(),
            early_draw: BTreeMap::new(),
            complete: RangeSetBlaze::new(),
            draw_state: DrawState::default(),
        }
    }

    fn report_fragment(&mut self, frag: FrameFragment) -> Result<()> {
        let encoded_frag = EncodedFragmentData::try_from(frag)?;
        trace!(
            "Received frame/decoder/sequence {}/{}/{}",
            encoded_frag.frame,
            encoded_frag.decoder,
            encoded_frag.sequence
        );
        ensure!(
            encoded_frag.decoder < self.areas.len(),
            "Invalid decoder {}",
            encoded_frag.decoder
        );
        ensure!(
            !self.complete.contains(encoded_frag.frame),
            "fragment for complete frame {}",
            encoded_frag.frame
        );

        let decoded_frag = encoded_frag.decode()?;
        let frame = decoded_frag.frame;
        let decoder = decoded_frag.decoder;
        let sequence = decoded_frag.sequence;
        let terminal = decoded_frag.terminal;

        if let Some(early_draw) = self.early_draw.get_mut(&frame) {
            debug!(
                "frame {} drawn early, drawing {}/{} @ decoder {}, {:?}/{:?} immediately",
                frame,
                decoder,
                sequence,
                decoded_frag.decoder,
                decoded_frag.decoder_relative_origin,
                decoded_frag.pixmap.size()
            );
            early_draw.mark_fragment_received(decoder, sequence, terminal)?;
            self.draw_state.queue(decoded_frag);
        } else {
            if let Some(&curr_max) = self.in_flight.keys().last() {
                for f in curr_max..frame {
                    self.in_flight
                        .entry(f)
                        .or_insert(InFlightFrame::new(&self.areas));
                }
            }
            self.in_flight
                .entry(frame)
                .or_insert(InFlightFrame::new(&self.areas))
                .add_fragment(decoded_frag)?;
        }

        if terminal {
            self.report_terminal(frame, decoder)?;
        }

        Ok(())
    }

    fn report_empty_frame_terminal(&mut self, term: EmptyFrameTerminal) -> Result<()> {
        trace!(
            "Received empty frame terminal frame/decoder {}/{}",
            term.frame,
            term.encoder
        );
        let decoder = term.encoder as usize;
        ensure!(decoder < self.areas.len(), "Invalid decoder {}", decoder);

        let frame = term.frame as usize;
        ensure!(
            !self.complete.contains(frame),
            "empty frame terminal for complete frame {}",
            frame
        );

        if let Some(early_draw) = self.early_draw.get_mut(&frame) {
            early_draw.mark_empty_frame_terminal_received(decoder)?;
        } else {
            self.in_flight
                .entry(frame)
                .or_insert(InFlightFrame::new(&self.areas))
                .terminate_empty(decoder)?;
        }

        self.report_terminal(frame, decoder)
    }

    fn report_terminal(&mut self, frame: usize, decoder: usize) -> Result<()> {
        ensure!(
            !self.complete.contains(frame),
            "terminal for complete frame"
        );
        ensure!(decoder < self.areas.len(), "invalid decoder {}", decoder);

        if let Some(early_draw) = self.early_draw.remove(&frame) {
            match early_draw.try_complete() {
                Ok(_) => {
                    debug!("frame {} drawn early, now complete", frame);
                    self.complete.insert(frame);
                }
                Err(early_draw) => {
                    self.early_draw.insert(frame, early_draw);
                }
            }
        } else if let Some(in_flight) = self.in_flight.remove(&frame) {
            match in_flight.try_complete() {
                Ok(complete) => {
                    trace!("frame {} complete", frame);
                    self.complete.insert(frame);

                    for df in complete.decoded_fragments {
                        self.draw_state.queue(df);
                    }

                    let still_in_flight = self.in_flight.split_off(&frame);
                    let needs_early_draw = mem::replace(&mut self.in_flight, still_in_flight);

                    for (id, in_flight_frame) in needs_early_draw {
                        trace!("drawing frame {} early", id);
                        let new_dfs = match in_flight_frame.force_complete() {
                            Ok(complete) => {
                                self.complete.insert(id);
                                complete.decoded_fragments
                            }
                            Err((early_draw, new_dfs)) => {
                                self.early_draw.insert(id, early_draw);
                                new_dfs
                            }
                        };

                        for df in new_dfs {
                            self.draw_state.queue(df);
                        }
                    }
                }
                Err(in_flight) => {
                    self.in_flight.insert(frame, in_flight);
                }
            }
        } else {
            unreachable!("frame is not complete, in flight, or early drawn")
        }

        Ok(())
    }

    pub fn report_mm(&mut self, msg: mm_s2c::Message) -> Result<()> {
        let res = match msg {
            mm_s2c::Message::Fragment(frag) => self.report_fragment(frag),
            mm_s2c::Message::Terminal(term) => self.report_empty_frame_terminal(term),
        };

        trace!(
            "in-flight/early-drawn/complete : {}/{}/{}",
            self.in_flight.len(),
            self.early_draw.len(),
            self.complete.len()
        );
        res
    }

    pub fn get_draws_and_reset(&mut self) -> Vec<Draw> {
        let draw_state = mem::take(&mut self.draw_state);

        let mut draws = draw_state
            .undrawn_fragments
            .into_iter()
            .map(|df| Draw {
                frame: df.frame,
                origin: df.decoder_relative_origin + self.areas[df.decoder].min.to_vector(),
                pixels: df.pixmap,
            })
            .collect::<Vec<_>>();
        draws.sort_by_key(|draw| draw.frame);
        for draw in &draws {
            trace!(
                "Drawing {} @ {:?}/{:?}",
                draw.frame,
                draw.origin,
                draw.pixels.size()
            );
        }
        // draws.reverse();
        draws
    }

    pub fn has_draws(&self) -> bool {
        !self.draw_state.is_empty()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use test_log::test;

    #[test]
    fn decode_zlib() {
        use flate2::read::ZlibEncoder;
        use flate2::{Compress, Compression};

        let data = vec![1; 2 * 3 * 3];
        let mut encoded = Vec::new();

        let mut z = ZlibEncoder::new_with_compress(
            data.as_slice(),
            Compress::new(Compression::new(9), false),
        );
        z.read_to_end(&mut encoded).unwrap();

        let decoded = decode(Codec::Zlib, encoded).expect("decode zlib");
        assert_eq!(decoded, data);
    }

    #[test]
    fn decode_raw() {
        let data = vec![0x5a, 2 * 3 * 3];
        let encoded = data.clone();
        let decoded = decode(Codec::Raw, encoded).expect("decode raw");
        assert_eq!(decoded, data);
    }
}
