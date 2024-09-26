use crate::metrics::{CompressionRatio, PixelsCompressed, TimeInCompression};
use crate::task::frame_sync::Frame;

use aperturec_graphics::prelude::*;
use aperturec_protocol::common::{Dimension as AcDimension, Location as AcLocation, *};
use aperturec_protocol::media::{self as mm, server_to_client as mm_s2c};
use aperturec_state_machine::*;

use anyhow::{anyhow, bail, Result};
use flate2::{write::DeflateEncoder, Compression};
use ndarray::prelude::*;
use std::io::Write;
use std::mem;
use std::slice;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::{self, JoinHandle};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::*;

fn do_encode_optimize_contig<F: FnOnce(&[u8]) -> Result<Vec<u8>>>(
    raw_data: ArrayView2<Pixel24>,
    f: F,
) -> Result<Vec<u8>> {
    if let Some(contig) = raw_data.as_slice() {
        // SAFETY: `raw_data` has a lifetime of at least the whole function
        // `do_encode_optimize_contig`, and the created slice is only valid for this if block, so
        // the data is guaranteed to live long enough.
        //
        // SAFETY: `raw_data` is of type `Pixel24` a 24-bit/3-byte struct. The constructed slice is
        // defined to be the number of pixels in the `contig` slice * size_of(Pixel24), so returned
        // slice is not out-of-bounds of `contig`
        let raw = unsafe {
            #[allow(clippy::manual_slice_size_calculation)]
            slice::from_raw_parts(
                contig as *const [Pixel24] as *const u8,
                contig.len() * mem::size_of::<Pixel24>(),
            )
        };
        f(raw)
    } else {
        let size = raw_data.size();
        let mut raw = Vec::with_capacity(size.area());

        let raw_as_ndarray = ArrayViewMut2::from_shape(size.as_shape(), raw.spare_capacity_mut())?;

        raw_data.assign_to(raw_as_ndarray);

        // SAFETY: Vec::set_len is safe because we do the assignment in the `assign_to` call above
        //
        // SAFETY: slice::from_raw_parts is safe because we ensure the slice size is no greater
        // than the allocated vector size
        unsafe {
            raw.set_len(size.area());
            f(slice::from_raw_parts(
                raw.as_ptr() as *const Pixel24 as *const u8,
                size.area() * mem::size_of::<Pixel24>(),
            ))
        }
    }
}

fn encode_raw(raw_data: ArrayView2<Pixel24>) -> Result<Vec<u8>> {
    do_encode_optimize_contig(raw_data, |bytes| Ok(bytes.to_vec()))
}

fn encode_zlib(raw_data: ArrayView2<Pixel24>) -> Result<Vec<u8>> {
    tokio::task::block_in_place(|| {
        do_encode_optimize_contig(raw_data, move |bytes| {
            let mut enc_data = vec![];
            {
                let mut compressor = DeflateEncoder::new(&mut enc_data, Compression::new(9));
                compressor.write_all(bytes)?;
                compressor.try_finish()?;
            }
            Ok(enc_data)
        })
    })
}

fn encode_jpegxl(raw_data: ArrayView2<Pixel24>) -> Result<Vec<u8>> {
    let width = raw_data.as_ndarray().len_of(axis::X);
    let height = raw_data.as_ndarray().len_of(axis::Y);
    let runner = jpegxl_rs::ThreadsRunner::default();
    let mut encoder = jpegxl_rs::encoder_builder()
        .quality(1.0)
        .speed(jpegxl_rs::encode::EncoderSpeed::Lightning)
        .decoding_speed(4_i64)
        .parallel_runner(&runner)
        .build()
        .expect("encoder build");

    Ok(tokio::task::block_in_place(|| {
        do_encode_optimize_contig(raw_data, move |bytes| {
            Ok(encoder
                .encode::<u8, u8>(&bytes, width as u32, height as u32)?
                .data)
        })
    })?)
}

fn encode(codec: Codec, raw_data: ArrayView2<Pixel24>) -> Result<Vec<u8>> {
    let size = raw_data.size();

    let start = Instant::now();
    let data = match codec {
        Codec::Raw => encode_raw(raw_data)?,
        Codec::Zlib => encode_zlib(raw_data)?,
        Codec::Jpegxl => encode_jpegxl(raw_data)?,
        Codec::Unspecified => bail!("Unspecified codec"),
    };
    let end = Instant::now();
    TimeInCompression::inc_by((end - start).as_secs_f64());
    PixelsCompressed::inc_by(size.area() as f64);

    let nbytes_produced = data.len();
    let nbytes_consumed = size.area() * mem::size_of::<Pixel24>();

    let ratio = 100_f64 * (nbytes_produced as f64 / nbytes_consumed as f64);
    CompressionRatio::observe(ratio);
    Ok(data)
}

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    id: usize,
    state: S,
}

#[derive(State)]
pub struct Created {
    codec: Codec,
    area: Box2D,
    frame_rx: mpsc::Receiver<Arc<Frame>>,
    mm_tx: mpsc::Sender<mm_s2c::Message>,
}

#[derive(State, Debug)]
pub struct Running {
    ct: CancellationToken,
    task: JoinHandle<Result<()>>,
}

#[derive(State, Debug)]
pub struct Terminated;

impl Task<Created> {
    pub fn new(
        id: usize,
        area: Box2D,
        codec: Codec,
        frame_rx: mpsc::Receiver<Arc<Frame>>,
        mm_tx: mpsc::Sender<mm_s2c::Message>,
    ) -> Self {
        Task {
            id,
            state: Created {
                codec,
                area,
                frame_rx,
                mm_tx,
            },
        }
    }
}

impl Task<Running> {
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.state.ct
    }
}

impl Transitionable<Running> for Task<Created> {
    type NextStateful = Task<Running>;

    fn transition(mut self) -> Self::NextStateful {
        let task: JoinHandle<Result<()>> = task::spawn(async move {
            while let Some(frame) = self.state.frame_rx.recv().await {
                let relevant = frame
                    .buffers
                    .iter()
                    .filter_map(|buffer| {
                        self.state
                            .area
                            .intersection(&buffer.area)
                            .map(|intersection| (buffer, intersection))
                    })
                    .collect::<Vec<_>>();

                if relevant.is_empty() {
                    self.state
                        .mm_tx
                        .send(mm_s2c::Message::Terminal(mm::EmptyFrameTerminal {
                            frame: frame.id as u64,
                            encoder: self.id as u32,
                        }))
                        .await?;
                    trace!(
                        "Dispatched frame/encoder/sequence {}/{}/<empty>",
                        frame.id,
                        self.id
                    );
                } else {
                    for (sequence, (buffer, intersection)) in relevant.iter().enumerate() {
                        let pixmap = buffer.pixels.as_ndarray();
                        let pixels = pixmap.slice(
                            intersection
                                .to_i64()
                                .translate(-buffer.area.min.to_vector().to_i64())
                                .to_usize()
                                .as_slice(),
                        );

                        let data = encode(self.state.codec, pixels)?;
                        let loc = intersection.min.to_vector() - self.state.area.min.to_vector();

                        let frag = mm::FrameFragment {
                            frame: frame.id as u64,
                            encoder: self.id as u32,
                            sequence: sequence as u32,
                            terminal: sequence == relevant.len() - 1,
                            codec: self.state.codec.into(),
                            location: Some(AcLocation::new(loc.x as u64, loc.y as u64)),
                            dimension: Some(AcDimension::new(
                                intersection.width() as u64,
                                intersection.height() as u64,
                            )),
                            data,
                        };

                        self.state
                            .mm_tx
                            .send(mm_s2c::Message::Fragment(frag))
                            .await?;

                        trace!(
                            "Dispatched frame/encoder/sequence {}/{}/{}",
                            frame.id,
                            self.id,
                            sequence
                        );
                    }
                }
            }
            bail!("[encoder {}] frame stream exhausted", self.id)
        });

        Task {
            id: self.id,
            state: Running {
                task,
                ct: CancellationToken::new(),
            },
        }
    }
}

impl AsyncTryTransitionable<Terminated, Terminated> for Task<Running> {
    type SuccessStateful = Task<Terminated>;
    type FailureStateful = Task<Terminated>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let abort_handle = self.state.task.abort_handle();
        let stateful = Task {
            id: self.id,
            state: Terminated,
        };
        tokio::select! {
            _ = self.state.ct.cancelled() => {
                abort_handle.abort();
                Ok(stateful)
            },
            task_res = self.state.task => {
                let error = match task_res {
                    Ok(Ok(())) => anyhow!("encoder task exited without internal error"),
                    Ok(Err(e)) => anyhow!("encoder task exited with internal error: {}", e),
                    Err(e) => anyhow!("encoder task exited with panic: {}", e),
                };
                Err(Recovered { stateful, error })
            },
        }
    }
}

impl Transitionable<Terminated> for Task<Running> {
    type NextStateful = Task<Terminated>;

    fn transition(self) -> Self::NextStateful {
        Task {
            id: self.id,
            state: Terminated,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const DATA: [Pixel24; 4096] = [Pixel24 {
        blue: 0,
        green: 255,
        red: 0,
    }; 4096];

    fn assert_eq_decoded(dec: &[u8]) {
        for (i, chunk) in dec.chunks(3).enumerate() {
            assert_eq!(chunk[0], DATA[i].blue);
            assert_eq!(chunk[1], DATA[i].green);
            assert_eq!(chunk[2], DATA[i].red);
        }
    }

    #[test]
    fn simple_encode_raw() {
        let raw_data = ArrayView2::from_shape((64, 64), &DATA).expect("create raw_data");
        let enc_data: Vec<_> = encode(Codec::Raw, raw_data).expect("encode raw");
        assert_eq!(enc_data.len(), 64 * 64 * 3);
        assert_eq_decoded(&enc_data);
    }

    #[test]
    fn simple_encode_zlib() {
        let raw_data = ArrayView2::from_shape((64, 64), &DATA).expect("create raw_data");
        let enc_data: Vec<_> = encode(Codec::Zlib, raw_data).expect("encode zlib");

        let mut decompressor = flate2::Decompress::new(false);
        let mut dec_data = vec![];
        decompressor
            .decompress_vec(&enc_data, &mut dec_data, flate2::FlushDecompress::Finish)
            .expect("decode zlib");
        assert_eq_decoded(&dec_data);
    }
}
