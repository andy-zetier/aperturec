use crate::metrics::{CompressionRatio, PixelsCompressed, TimeInCompression};
use crate::task::frame_sync::Frame;

use aperturec_graphics::prelude::*;
use aperturec_protocol::common::{Dimension as AcDimension, Location as AcLocation, *};
use aperturec_protocol::media::{self as mm, server_to_client as mm_s2c};
use aperturec_state_machine::*;

use anyhow::{anyhow, bail, Result};
use flate2::{write::DeflateEncoder, Compression};
use futures::{prelude::*, stream::FuturesUnordered};
use ndarray::prelude::*;
use std::io::Write;
use std::mem;
use std::slice;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio::task::{self, JoinHandle};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::*;

pub enum Command {
    UpdateArea(Box2D),
}

mod compression {
    use super::*;

    pub enum Scheme {
        Raw,
        Zlib(zlib::Compressor),
        Jpegxl(jxl::Compressor),
    }

    impl Scheme {
        pub async fn compress(&self, raw_data: ArrayView2<'_, Pixel24>) -> Result<Vec<u8>> {
            match self {
                Scheme::Raw => Ok(task::block_in_place(move || {
                    do_compress_optimize_contig(raw_data, |bytes| bytes.to_vec())
                })),
                Scheme::Zlib(compressor) => compressor.compress(raw_data).await,
                Scheme::Jpegxl(compressor) => compressor.compress(raw_data).await,
            }
        }

        pub fn as_codec(&self) -> Codec {
            match self {
                Scheme::Raw => Codec::Raw,
                Scheme::Zlib(_) => Codec::Zlib,
                Scheme::Jpegxl(_) => Codec::Jpegxl,
            }
        }
    }

    impl TryFrom<Codec> for Scheme {
        type Error = anyhow::Error;

        fn try_from(codec: Codec) -> Result<Self, Self::Error> {
            match codec {
                Codec::Raw => Ok(Scheme::Raw),
                Codec::Zlib => Ok(Scheme::Zlib(zlib::Compressor::default())),
                Codec::Jpegxl => Ok(Scheme::Jpegxl(jxl::Compressor::default())),
                _ => bail!("Unsupported codec"),
            }
        }
    }

    fn do_compress_optimize_contig<C, F: FnOnce(&[u8]) -> C>(
        raw_data: ArrayView2<Pixel24>,
        f: F,
    ) -> C {
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

            let raw_as_ndarray =
                ArrayViewMut2::from_shape(size.as_shape(), raw.spare_capacity_mut())
                    .expect("ndarray shape");

            raw_data.assign_to(raw_as_ndarray);

            // SAFETY: Vec::set_len is safe because we do the assignment in the `assign_to` call above
            //
            // SAFETY: slice::from_raw_parts is safe because we ensure the slice size is no greater
            // than the allocated vector size
            unsafe {
                raw.set_len(size.area());
                f(slice::from_raw_parts(
                    raw.as_ptr() as *const u8,
                    size.area() * mem::size_of::<Pixel24>(),
                ))
            }
        }
    }

    pub mod zlib {
        use super::*;
        pub struct Compressor {
            encoder: AsyncMutex<DeflateEncoder<Vec<u8>>>,
        }

        impl Default for Compressor {
            fn default() -> Self {
                Self {
                    encoder: AsyncMutex::new(DeflateEncoder::new(Vec::new(), Compression::new(9))),
                }
            }
        }

        impl Compressor {
            pub async fn compress(&self, raw_data: ArrayView2<'_, Pixel24>) -> Result<Vec<u8>> {
                let mut encoder = self.encoder.lock().await;
                task::block_in_place(|| {
                    do_compress_optimize_contig(raw_data, move |bytes| {
                        encoder.write_all(bytes)?;
                        encoder.try_finish()?;
                        Ok(encoder.reset(vec![])?)
                    })
                })
            }
        }
    }

    pub mod jxl {
        use super::*;

        use jpegxl_rs::encode::JxlEncoder;

        struct SendEncoder {
            inner: JxlEncoder<'static, 'static>,
        }

        // SAFETY: `SendEncoder` is not `Send` because `JxlEncoder` has raw pointers. However, accesses to
        // raw pointers can be safely `Send` if they are exclusive. The raw pointers within
        // `JxlEncoder` are allocated by libjxl and libjxl does not provide access to these
        // pointers (the `JxlEncoder` struct is opaque).
        unsafe impl Send for SendEncoder {}

        pub struct Compressor {
            encoder: AsyncMutex<SendEncoder>,
        }

        impl Default for Compressor {
            fn default() -> Self {
                Self {
                    encoder: AsyncMutex::new(SendEncoder {
                        inner: jpegxl_rs::encode::JxlEncoderBuilder::default()
                            .quality(1.0)
                            .speed(jpegxl_rs::encode::EncoderSpeed::Lightning)
                            .decoding_speed(4_i64)
                            .build()
                            .expect("create encoder"),
                    }),
                }
            }
        }

        impl Compressor {
            pub async fn compress(&self, raw_data: ArrayView2<'_, Pixel24>) -> Result<Vec<u8>> {
                let width = raw_data.len_of(axis::X) as u32;
                let height = raw_data.len_of(axis::Y) as u32;

                let mut encoder = self.encoder.lock().await;
                task::block_in_place(|| {
                    Ok(do_compress_optimize_contig(raw_data, |bytes| {
                        encoder.inner.encode::<u8, u8>(bytes, width, height)
                    })?
                    .data)
                })
            }
        }
    }
}

#[derive(Stateful, SelfTransitionable, Debug)]
#[state(S)]
pub struct Task<S: State> {
    id: usize,
    state: S,
}

#[derive(State)]
pub struct Created {
    compression_scheme: compression::Scheme,
    area: Box2D,
    frame_rx: mpsc::Receiver<Arc<Frame>>,
    mm_tx: mpsc::Sender<mm_s2c::Message>,
    command_rx: mpsc::Receiver<Command>,
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
    ) -> Result<(Self, mpsc::Sender<Command>)> {
        let compression_scheme = codec.try_into()?;
        let (command_tx, command_rx) = mpsc::channel(1);
        Ok((
            Task {
                id,
                state: Created {
                    compression_scheme,
                    area,
                    frame_rx,
                    mm_tx,
                    command_rx,
                },
            },
            command_tx,
        ))
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
        let encoder_id = self.id;
        let task: JoinHandle<Result<()>> = task::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    Some(command) = self.state.command_rx.recv() => {
                        let Command::UpdateArea(area) = command;
                        debug!(encoder=%encoder_id, old_area=?self.state.area, new_area=?area, "Update area");
                        self.state.area = area;
                    },
                    Some(frame) = self.state.frame_rx.recv() => {
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
                                    encoder: encoder_id as u32,
                                }))
                            .await?;
                            trace!(frame=%frame.id, encoder=%encoder_id, "Dispatched empty frame");
                        } else {
                            let mut encodings = FuturesUnordered::new();
                            for (buffer, intersection) in &relevant {
                                encodings.push(async {
                                    let buffer_relative_area = intersection
                                        .to_i64()
                                        .translate(-buffer.area.min.to_vector().to_i64())
                                        .to_usize();
                                    let enc_relative_area = intersection
                                        .to_i64()
                                        .translate(-self.state.area.min.to_vector().to_i64())
                                        .to_usize();

                                    let pixmap = buffer.pixels.as_ndarray();
                                    let pixels = pixmap.slice(buffer_relative_area.as_slice());

                                    let size = pixels.size();
                                    let start = Instant::now();
                                    let data = self.state.compression_scheme.compress(pixels).await?;
                                    let end = Instant::now();
                                    TimeInCompression::inc_by((end - start).as_secs_f64());
                                    PixelsCompressed::inc_by(size.area() as f64);

                                    let nbytes_produced = data.len();
                                    let nbytes_consumed = size.area() * mem::size_of::<Pixel24>();

                                    let ratio = 100_f64 * (nbytes_produced as f64 / nbytes_consumed as f64);
                                    CompressionRatio::observe(ratio);

                                    Ok::<_, anyhow::Error>((data, enc_relative_area))
                                });
                            }

                            let mut sequence = 0;
                            while let Some(Ok((data, enc_relative_area))) = encodings.next().await {
                                let frag = mm::FrameFragment {
                                    frame: frame.id as u64,
                                    encoder: encoder_id as u32,
                                    sequence: sequence as u32,
                                    terminal: sequence == relevant.len() - 1,
                                    codec: self.state.compression_scheme.as_codec().into(),
                                    location: Some(AcLocation::new(
                                            enc_relative_area.min.x as u64,
                                            enc_relative_area.min.y as u64,
                                    )),
                                    dimension: Some(AcDimension::new(
                                            enc_relative_area.width() as u64,
                                            enc_relative_area.height() as u64,
                                    )),
                                    data,
                                };

                                self.state
                                    .mm_tx
                                    .send(mm_s2c::Message::Fragment(frag))
                                    .await?;

                                trace!(frame=%frame.id, encoder=%encoder_id, sequence, "Dispatched frame fragment");

                                sequence += 1;
                            }
                        }
                    }
                    else => bail!("[encoder {}] exhausted", self.id),
                }
            }
        });

        Task {
            id: encoder_id,
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
                debug!(encoder=self.id, "task cancelled");
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
    use test_log::test;

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

    #[test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
    async fn simple_encode_raw() {
        let raw_data = ArrayView2::from_shape((64, 64), &DATA).expect("create raw_data");
        let enc_data: Vec<_> = compression::Scheme::Raw
            .compress(raw_data)
            .await
            .expect("encode raw");
        assert_eq!(enc_data.len(), 64 * 64 * 3);
        assert_eq_decoded(&enc_data);
    }

    #[test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
    async fn simple_encode_zlib() {
        let raw_data = ArrayView2::from_shape((64, 64), &DATA).expect("create raw_data");
        let enc_data: Vec<_> = compression::Scheme::try_from(Codec::Zlib)
            .expect("create zlib scheme")
            .compress(raw_data)
            .await
            .expect("encode zlib");
        let mut decompressor = flate2::Decompress::new(false);
        let mut dec_data = vec![];
        decompressor
            .decompress_vec(&enc_data, &mut dec_data, flate2::FlushDecompress::Finish)
            .expect("decode zlib");
        assert_eq_decoded(&dec_data);
    }
}
