use aperturec_graphics::{display::*, prelude::*};
use aperturec_protocol::common::Codec;
use aperturec_protocol::media::{EmptyFrameTerminal, FrameFragment, server_to_client as mm_s2c};

use flate2::read::DeflateDecoder;
use ndarray::prelude::*;
use range_set_blaze::RangeSetBlaze;
use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::io::Read;
use std::mem::{self, MaybeUninit};
use std::num::TryFromIntError;
use std::ptr;
use thiserror::Error;
use tracing::*;

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("unsupported codec: {0:?}")]
    UnsupportedCodec(Codec),
    #[error("missing fragment origin")]
    MissingFragmentOrigin,
    #[error("missing fragment dimension")]
    MissingFragmentDimension,
    #[error("invalid codec value: {0}")]
    InvalidCodecValue(i32),
    #[error("decompressed length {actual} != {width}x{height}x{pixel_size} = {expected}")]
    PixelDataLengthMismatch {
        actual: usize,
        expected: usize,
        width: usize,
        height: usize,
        pixel_size: usize,
    },
    #[error("identical sequence {sequence}")]
    DuplicateSequence { sequence: usize },
    #[error("received duplicate terminals")]
    DuplicateTerminal,
    #[error("marking sequence received for complete decoder frame")]
    SequenceForCompleteDecoderFrame,
    #[error("marking non-empty frame terminated with empty frame terminal")]
    EmptyFrameTerminalNonEmpty,
    #[error("marking empty frame terminal received for previously terminated frame")]
    EmptyFrameTerminalAfterTermination,
    #[error("invalid decoder {decoder}")]
    InvalidDecoder { decoder: usize },
    #[error("mark fragment {sequence} received for decoder {decoder}, which does not exist")]
    InvalidDecoderForFragment { decoder: usize, sequence: usize },
    #[error("mark empty frame terminal received for decoder {decoder}, which does not exist")]
    InvalidDecoderForEmptyTerminal { decoder: usize },
    #[error("mismatching display config id {actual} != {expected}")]
    MismatchedDisplayConfig { expected: usize, actual: usize },
    #[error("invalid display {display}")]
    InvalidDisplay { display: usize },
    #[error("jxl decoder samples are not {expected} bit")]
    JxlBitsPerSample { expected: u32, actual: u32 },
    #[error("jxl decoder samples are floating point")]
    JxlExponentBitsPerSample { expected: u32, actual: u32 },
    #[error("jxl decoder producing animation")]
    JxlHasAnimation,
    #[error("jxl decoder has {actual} color channels")]
    JxlColorChannels { expected: u32, actual: u32 },
    #[error("jxl decoder has {actual} extra channels")]
    JxlExtraChannels { expected: u32, actual: u32 },
    #[error("jpegxl decoder error: {0}")]
    Jxl(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("integer conversion failed: {0}")]
    TryFromInt(#[from] TryFromIntError),
    #[error("invalid pixel buffer shape: {0}")]
    Shape(#[from] ndarray::ShapeError),
}

type Result<T, E = FrameError> = std::result::Result<T, E>;

fn decode_raw(encoded_data: Vec<u8>) -> Result<Vec<u8>> {
    Ok(encoded_data)
}

fn decode_zlib(encoded_data: Vec<u8>) -> Result<Vec<u8>> {
    let mut z = DeflateDecoder::new(&*encoded_data);
    let mut decoded = vec![];
    z.read_to_end(&mut decoded)?;
    Ok(decoded)
}

fn decode_jpegxl(encoded_data: Vec<u8>) -> Result<Vec<u8>> {
    use aperturec_utils::jxl::*;
    use jxl_sys::*;

    // SAFETY:
    //   - JxlDecoderCreate can take a null pointer to skip using a custom memory manager
    //   - jxl_call! checks the return value of JxlDecoderCreate and bails if it fails
    let dec =
        jxl_call!(JxlDecoderCreate, ptr::null()).map_err(|e| FrameError::Jxl(e.to_string()))?;

    // SAFETY:
    //   - JxlResizableParallelRunnerCreate can take a null pointer to skip using a custom memory
    //   manager
    //   - jxl_call! checks the return value of JxlResizableParallelRunnerCreate and bails if it
    //   fails
    let par_runner_opaque = jxl_call!(JxlResizableParallelRunnerCreate, ptr::null())
        .map_err(|e| FrameError::Jxl(e.to_string()))?;

    // SAFETY:
    //   - dec is null-checked above
    //   - par_runner_opaque is null-checked above
    jxl_call!(
        JxlDecoderSetParallelRunner,
        dec,
        Some(JxlResizableParallelRunner),
        par_runner_opaque
    )
    .map_err(|e| FrameError::Jxl(e.to_string()))?;

    // SAFETY:
    //   - dec is null-checked above
    //   - encoded_data is a rust vec, therefore having a valid pointer and length
    //   - jxl_call! checks the return value of JxlDecoderSetInput and bails if it fails
    jxl_call!(
        JxlDecoderSetInput,
        dec,
        encoded_data.as_ptr(),
        encoded_data.len()
    )
    .map_err(|e| FrameError::Jxl(e.to_string()))?;
    // SAFETY:
    //   - dec is null-checked above
    unsafe { JxlDecoderCloseInput(dec as _) };

    // SAFETY:
    //   - dec is null-checked above
    //   - jxl_call! checks the return value of JxlDecoderSubscribeEvents and bails if it fails
    jxl_call!(
        JxlDecoderSubscribeEvents,
        dec,
        (JxlDecoderStatus::JXL_DEC_BASIC_INFO as i32)
            | (JxlDecoderStatus::JXL_DEC_FULL_IMAGE as i32),
    )
    .map_err(|e| FrameError::Jxl(e.to_string()))?;

    let info: OnceCell<JxlBasicInfo> = OnceCell::new();
    let output_size: OnceCell<usize> = OnceCell::new();
    let mut output: OnceCell<Vec<u8>> = OnceCell::new();
    loop {
        // SAFETY:
        //   - dec is null-checked above
        let status = unsafe { JxlDecoderProcessInput(dec) };
        match status {
            JxlDecoderStatus::JXL_DEC_SUCCESS => {
                break;
            }
            JxlDecoderStatus::JXL_DEC_ERROR => {
                return Err(FrameError::Jxl("jxl decoder exited with error".to_string()));
            }
            JxlDecoderStatus::JXL_DEC_NEED_MORE_INPUT => {
                return Err(FrameError::Jxl(
                    "jxl decoder needed more input but all input provided".to_string(),
                ));
            }
            JxlDecoderStatus::JXL_DEC_FULL_IMAGE => {
                let Some(output) = output.get_mut() else {
                    return Err(FrameError::Jxl(
                        "jxl decoder finished without setting output".to_string(),
                    ));
                };
                let Some(output_size) = output_size.get() else {
                    return Err(FrameError::Jxl(
                        "jxl decoder finished without setting output size".to_string(),
                    ));
                };
                if output.capacity() < *output_size {
                    return Err(FrameError::Jxl(
                        "jxl decoder finished with an output size greater than the capacity of the output buffer"
                            .to_string(),
                    ));
                }
                // SAFETY:
                //   - We check above to ensure that the capacity of the output is at least the
                //   provided size
                unsafe { output.set_len(*output_size) };
            }
            JxlDecoderStatus::JXL_DEC_BASIC_INFO => {
                let mut info_uninit: MaybeUninit<JxlBasicInfo> = MaybeUninit::uninit();
                // SAFETY:
                //   - dec is null-checked above
                //   - info_uninit is allocated (uninitialized) on the stack and guaranteed to have
                //   a valid pointer
                //   - jxl_call! checks the return value of JxlDecoderGetBasicInfo and bails if it
                //   fails
                jxl_call!(JxlDecoderGetBasicInfo, dec, info_uninit.as_mut_ptr())
                    .map_err(|e| FrameError::Jxl(e.to_string()))?;
                // SAFETY:
                //   - info is initialized by JxlDecoderGetBasicInfo
                if info.set(unsafe { info_uninit.assume_init() }).is_err() {
                    return Err(FrameError::Jxl(
                        "jxl decoder received basic info more than once".to_string(),
                    ));
                }
                let info = info.get().unwrap();
                if info.bits_per_sample != parameters::BITS_PER_SAMPLE {
                    return Err(FrameError::JxlBitsPerSample {
                        expected: parameters::BITS_PER_SAMPLE,
                        actual: info.bits_per_sample,
                    });
                }
                if info.exponent_bits_per_sample != parameters::EXPONENT_BITS_PER_SAMPLE {
                    return Err(FrameError::JxlExponentBitsPerSample {
                        expected: parameters::EXPONENT_BITS_PER_SAMPLE,
                        actual: info.exponent_bits_per_sample,
                    });
                }
                if info.have_animation != 0 {
                    return Err(FrameError::JxlHasAnimation);
                }
                if info.num_color_channels != PIXEL_FORMAT.num_channels {
                    return Err(FrameError::JxlColorChannels {
                        expected: PIXEL_FORMAT.num_channels,
                        actual: info.num_color_channels,
                    });
                }
                if info.num_extra_channels != NO_EXTRA_CHANNELS_FORMAT.num_channels {
                    return Err(FrameError::JxlExtraChannels {
                        expected: NO_EXTRA_CHANNELS_FORMAT.num_channels,
                        actual: info.num_extra_channels,
                    });
                }

                // SAFETY: JxlResizableParallelRunnerSuggestThreads is only marked unsafe at it is
                // an external C++ function. Nothing about the function is inherently unsafe
                let num_threads = unsafe {
                    JxlResizableParallelRunnerSuggestThreads(info.xsize as _, info.ysize as _)
                };
                // SAFETY:
                //   - par_runner_opaque is guaranteed to be non-null as it is a reference
                unsafe {
                    JxlResizableParallelRunnerSetThreads(par_runner_opaque, num_threads as _)
                };
            }
            JxlDecoderStatus::JXL_DEC_NEED_IMAGE_OUT_BUFFER => {
                let mut buffer_size = 0;
                // SAFETY:
                //   - dec is null-checked above
                //   - the other arguments are references and therefore guaranteed non-null
                //   - jxl_call! checks the return value of JxlDecoderImageOutBufferSize and bails
                //   if it fails
                jxl_call!(
                    JxlDecoderImageOutBufferSize,
                    dec,
                    &PIXEL_FORMAT,
                    &mut buffer_size
                )
                .map_err(|e| FrameError::Jxl(e.to_string()))?;
                let Some(info) = info.get() else {
                    return Err(FrameError::Jxl(
                        "jxl decoder needs image output buffer before basic info received"
                            .to_string(),
                    ));
                };
                let expected_size = (info.xsize * info.ysize * PIXEL_FORMAT.num_channels) as usize;
                if buffer_size != expected_size {
                    return Err(FrameError::Jxl(format!(
                        "jxl decoder output buffer size is invalid - expected {expected_size}, got {buffer_size}"
                    )));
                }
                if output_size.set(expected_size).is_err() {
                    return Err(FrameError::Jxl(
                        "jxl decoder set output size more than once".to_string(),
                    ));
                }
                if output.set(Vec::with_capacity(expected_size)).is_err() {
                    return Err(FrameError::Jxl(
                        "jxl decoder set output more than once".to_string(),
                    ));
                }
                let uninit_slice = output.get_mut().unwrap().spare_capacity_mut();
                // SAFETY:
                //   - dec is null-checked above
                //   - uninit slice is initialized with a capacity of expected_size so it is
                //   guaranteed to exist and be at least expected_size length
                jxl_call!(
                    JxlDecoderSetImageOutBuffer,
                    dec,
                    &PIXEL_FORMAT,
                    uninit_slice.as_mut_ptr() as *mut _,
                    expected_size,
                )
                .map_err(|e| FrameError::Jxl(e.to_string()))?;
            }
            event => {
                return Err(FrameError::Jxl(format!(
                    "unexpected event from jxl decoder: {event:?}"
                )));
            }
        }
    }

    // SAFETY:
    //   - Decoder is done being used and can be destroyed
    unsafe { JxlDecoderDestroy(dec) };

    // SAFETY:
    //   - Parallel runner is done being used and can be destroyed
    unsafe { JxlResizableParallelRunnerDestroy(par_runner_opaque) };

    output
        .take()
        .ok_or_else(|| FrameError::Jxl("jxl decoder output never set".to_string()))
}

fn decode(codec: Codec, encoded_data: Vec<u8>) -> Result<Vec<u8>> {
    match codec {
        Codec::Raw => decode_raw(encoded_data),
        Codec::Zlib => decode_zlib(encoded_data),
        Codec::Jpegxl => decode_jpegxl(encoded_data),
        codec => Err(FrameError::UnsupportedCodec(codec)),
    }
}

#[derive(Debug, Clone)]
struct EncodedFragmentData {
    frame: usize,
    decoder: usize,
    sequence: usize,
    display: usize,
    display_config: usize,
    terminal: bool,
    codec: Codec,
    size: Size,
    decoder_relative_origin: Point,
    encoded_pixels: Vec<u8>,
}

impl TryFrom<FrameFragment> for EncodedFragmentData {
    type Error = FrameError;

    fn try_from(frag: FrameFragment) -> Result<Self> {
        let loc = frag.location.ok_or(FrameError::MissingFragmentOrigin)?;
        let dim = frag.dimension.ok_or(FrameError::MissingFragmentDimension)?;
        let origin = Point::new(loc.x_position.try_into()?, loc.y_position.try_into()?);
        let size = Size::new(dim.width.try_into()?, dim.height.try_into()?);
        let codec =
            Codec::try_from(frag.codec).map_err(|_| FrameError::InvalidCodecValue(frag.codec))?;

        Ok(EncodedFragmentData {
            frame: frag.frame.try_into()?,
            decoder: frag.encoder.try_into()?,
            sequence: frag.sequence.try_into()?,
            display: frag.display.try_into()?,
            display_config: frag.display_config.try_into()?,
            terminal: frag.terminal,
            codec,
            size,
            decoder_relative_origin: origin,
            encoded_pixels: frag.data,
        })
    }
}

impl EncodedFragmentData {
    fn decode(self) -> Result<DecodedFragmentData> {
        let decoded_data = decode(self.codec, self.encoded_pixels)?;
        if decoded_data.len() != self.size.area() * mem::size_of::<Pixel24>() {
            let expected = self.size.area() * mem::size_of::<Pixel24>();
            return Err(FrameError::PixelDataLengthMismatch {
                actual: decoded_data.len(),
                expected,
                width: self.size.width,
                height: self.size.height,
                pixel_size: mem::size_of::<Pixel24>(),
            });
        }

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
                if received.contains(sequence) {
                    return Err(FrameError::DuplicateSequence { sequence });
                }

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
                if terminal {
                    return Err(FrameError::DuplicateTerminal);
                }

                missing_sequences.remove(sequence);
                if missing_sequences.is_empty() {
                    *self = DecoderFrameState::Complete
                }
            }
            DecoderFrameState::Complete => {
                return Err(FrameError::SequenceForCompleteDecoderFrame);
            }
        }
        Ok(())
    }

    fn mark_empty_frame_terminal_received(&mut self) -> Result<()> {
        match self {
            DecoderFrameState::InFlight { received } => {
                if !received.is_empty() {
                    return Err(FrameError::EmptyFrameTerminalNonEmpty);
                }
                *self = DecoderFrameState::Complete;
                Ok(())
            }
            _ => Err(FrameError::EmptyFrameTerminalAfterTermination),
        }
    }
}

#[derive(Debug)]
struct InFlightFrame {
    decoder_frame_states: Box<[DecoderFrameState]>,
    decoded_fragments: Vec<DecodedFragmentData>,
}

impl InFlightFrame {
    fn new(areas: &[Rect]) -> Self {
        InFlightFrame {
            decoder_frame_states: vec![DecoderFrameState::default(); areas.len()]
                .into_boxed_slice(),
            decoded_fragments: vec![],
        }
    }

    fn add_fragment(&mut self, frag: DecodedFragmentData) -> Result<()> {
        if frag.decoder >= self.decoder_frame_states.len() {
            return Err(FrameError::InvalidDecoder {
                decoder: frag.decoder,
            });
        }
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
        if decoder >= self.decoder_frame_states.len() {
            return Err(FrameError::InvalidDecoder { decoder });
        }
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
        if decoder >= self.decoder_frame_states.len() {
            return Err(FrameError::InvalidDecoderForFragment { decoder, sequence });
        }

        self.decoder_frame_states[decoder].mark_sequence_received(sequence, terminal)?;
        Ok(())
    }

    fn mark_empty_frame_terminal_received(&mut self, decoder: usize) -> Result<()> {
        if decoder >= self.decoder_frame_states.len() {
            return Err(FrameError::InvalidDecoderForEmptyTerminal { decoder });
        }

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

/// Screen update with pixel data to render.
///
/// Represents a rectangular region of pixels that should be drawn to the screen.
#[derive(Debug)]
pub struct Draw {
    /// Frame sequence number.
    pub frame: usize,
    /// Top-left position in screen coordinates.
    pub origin: Point,
    /// Decoded pixel data in 24-bit RGB format.
    pub pixels: Pixel24Map,
}

impl Draw {
    /// Returns the rectangular area covered by this draw operation.
    pub fn area(&self) -> Rect {
        Rect::new(self.origin, self.pixels.size())
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

pub struct DisplayFramer {
    area: Rect,
    decoder_areas: Box<[Rect]>,
    in_flight: BTreeMap<usize, InFlightFrame>,
    early_draw: BTreeMap<usize, EarlyDrawFrame>,
    complete: RangeSetBlaze<usize>,
    draw_state: DrawState,
}

impl DisplayFramer {
    fn new(area: Rect, decoder_areas: &[Rect]) -> Self {
        DisplayFramer {
            area,
            decoder_areas: decoder_areas.to_vec().into_boxed_slice(),
            in_flight: BTreeMap::new(),
            early_draw: BTreeMap::new(),
            complete: RangeSetBlaze::new(),
            draw_state: DrawState::default(),
        }
    }

    fn report_fragment(&mut self, encoded_frag: EncodedFragmentData) -> Result<()> {
        if encoded_frag.decoder >= self.decoder_areas.len() {
            return Err(FrameError::InvalidDecoder {
                decoder: encoded_frag.decoder,
            });
        }
        if self.complete.contains(encoded_frag.frame) {
            trace!(
                "dropping fragment for complete frame {}",
                encoded_frag.frame
            );
            return Ok(());
        }

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
                        .or_insert(InFlightFrame::new(&self.decoder_areas));
                }
            }
            self.in_flight
                .entry(frame)
                .or_insert(InFlightFrame::new(&self.decoder_areas))
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
            term.frame, term.encoder
        );
        let decoder = term.encoder as usize;
        if decoder >= self.decoder_areas.len() {
            return Err(FrameError::InvalidDecoder { decoder });
        }

        let frame = term.frame as usize;
        if self.complete.contains(frame) {
            trace!("dropping empty frame terminal for complete frame {frame}");
            return Ok(());
        }

        if let Some(early_draw) = self.early_draw.get_mut(&frame) {
            early_draw.mark_empty_frame_terminal_received(decoder)?;
        } else {
            self.in_flight
                .entry(frame)
                .or_insert(InFlightFrame::new(&self.decoder_areas))
                .terminate_empty(decoder)?;
        }

        self.report_terminal(frame, decoder)
    }

    fn report_terminal(&mut self, frame: usize, decoder: usize) -> Result<()> {
        if self.complete.contains(frame) {
            trace!("dropping terminal for complete frame {frame}");
            return Ok(());
        }
        if decoder >= self.decoder_areas.len() {
            return Err(FrameError::InvalidDecoder { decoder });
        }

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

    fn get_draws_and_reset(&mut self) -> impl Iterator<Item = Draw> + use<'_> {
        let draw_state = mem::take(&mut self.draw_state);

        draw_state.undrawn_fragments.into_iter().map(|df| Draw {
            frame: df.frame,
            origin: df.decoder_relative_origin + self.decoder_areas[df.decoder].origin.to_vector(),
            pixels: df.pixmap,
        })
    }

    fn has_draws(&self) -> bool {
        !self.draw_state.is_empty()
    }
}

pub struct Framer {
    pub display_config: DisplayConfiguration,
    displays: Box<[DisplayFramer]>,
}

impl Framer {
    pub fn new(config: DisplayConfiguration) -> Self {
        Framer {
            displays: config
                .display_decoder_infos
                .iter()
                .map(|d| DisplayFramer::new(d.display.area, &d.decoder_areas))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            display_config: config,
        }
    }

    pub fn in_flight(&self) -> usize {
        self.displays.iter().map(|d| d.in_flight.len()).sum()
    }

    pub fn early_draw(&self) -> usize {
        self.displays.iter().map(|d| d.early_draw.len()).sum()
    }

    pub fn complete(&self) -> usize {
        self.displays.iter().map(|d| d.complete.len()).sum::<u128>() as usize
    }

    pub fn report_mm(&mut self, msg: mm_s2c::Message) -> Result<()> {
        let res = match msg {
            mm_s2c::Message::Fragment(frag) => self.report_fragment(frag),
            mm_s2c::Message::Terminal(term) => self.report_empty_frame_terminal(term),
        };

        trace!(
            in_flight = self.in_flight(),
            early_draw = self.early_draw(),
            complete = self.complete(),
        );
        res
    }

    fn report_fragment(&mut self, frag: FrameFragment) -> Result<()> {
        let encoded_frag = EncodedFragmentData::try_from(frag)?;
        trace!(
            display_config = encoded_frag.display_config,
            display = encoded_frag.display,
            frame = encoded_frag.frame,
            decoder = encoded_frag.decoder,
            sequence = encoded_frag.sequence,
            "received"
        );
        if encoded_frag.display_config < self.display_config.id {
            return Ok(());
        }
        if encoded_frag.display_config != self.display_config.id {
            return Err(FrameError::MismatchedDisplayConfig {
                expected: self.display_config.id,
                actual: encoded_frag.display_config,
            });
        }
        if encoded_frag.display >= self.displays.len() {
            return Err(FrameError::InvalidDisplay {
                display: encoded_frag.display,
            });
        }
        self.displays[encoded_frag.display].report_fragment(encoded_frag)
    }

    fn report_empty_frame_terminal(&mut self, term: EmptyFrameTerminal) -> Result<()> {
        trace!(
            display_config = term.display_config,
            display = term.display,
            frame = term.frame,
            decoder = term.encoder,
            "received empty frame terminal"
        );
        let display_config = term.display_config as usize;
        if display_config < self.display_config.id {
            return Ok(());
        }
        if display_config != self.display_config.id {
            return Err(FrameError::MismatchedDisplayConfig {
                expected: self.display_config.id,
                actual: display_config,
            });
        }
        let display = term.display as usize;
        if display >= self.displays.len() {
            return Err(FrameError::InvalidDisplay { display });
        }
        self.displays[display].report_empty_frame_terminal(term)
    }

    pub fn get_draws_and_reset(&mut self) -> impl Iterator<Item = Draw> + use<'_> {
        self.displays.iter_mut().flat_map(|df: &mut DisplayFramer| {
            let display_origin = df.area.origin;
            df.get_draws_and_reset().map(move |draw: Draw| Draw {
                frame: draw.frame,
                origin: display_origin + draw.origin.to_vector(),
                pixels: draw.pixels,
            })
        })
    }

    pub fn has_draws(&self) -> bool {
        self.displays.iter().any(DisplayFramer::has_draws)
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
