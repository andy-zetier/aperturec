use crate::metrics::{CompressionRatio, PixelsCompressed, TimeInCompression};
use crate::task::frame_sync::Frame;

use aperturec_graphics::prelude::*;
use aperturec_protocol::common::{Dimension as AcDimension, Location as AcLocation, *};
use aperturec_protocol::media::{self as mm, server_to_client as mm_s2c};
use aperturec_state_machine::*;

use anyhow::{Result, anyhow, bail, ensure};
use flate2::{Compression, write::DeflateEncoder};
use futures::{prelude::*, stream::FuturesUnordered};
use ndarray::prelude::*;
use std::ffi::c_void;
use std::io::Write;
use std::mem::{self, MaybeUninit};
use std::ptr;
use std::slice;
use std::sync::Arc;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::{self, JoinHandle};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::*;

pub enum Command {
    Reconfigure((usize, Box2D)),
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
                Codec::Jpegxl => Ok(Scheme::Jpegxl(jxl::Compressor::new()?)),
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
        use aperturec_utils::jxl::*;

        use jxl_sys::*;

        struct InputSource<'b> {
            bytes: &'b [u8],
            size: Size,
            stride_to_next_row: usize,
        }

        impl<'b> InputSource<'b> {
            fn new(bytes: &'b [u8], size: Size, stride_to_next_row: usize) -> Result<Self> {
                ensure!(
                    bytes.len()
                        == (size.width + stride_to_next_row)
                            * size.height
                            * mem::size_of::<Pixel24>(),
                    "invalid input source size"
                );
                Ok(InputSource {
                    bytes,
                    size,
                    stride_to_next_row,
                })
            }

            // SAFETY: null check relevant parameters and return null if any of them are null
            unsafe extern "C" fn get_color_channels_pixel_format_extern(
                _: *mut c_void,
                pixel_format: *mut JxlPixelFormat,
            ) {
                unsafe {
                    const PIXEL_FORMAT: JxlPixelFormat = JxlPixelFormat {
                        num_channels: mem::size_of::<Pixel24>() as _,
                        data_type: JxlDataType::JXL_TYPE_UINT8,
                        endianness: JxlEndianness::JXL_NATIVE_ENDIAN,
                        align: 0,
                    };
                    if let Some(pixel_format) = pixel_format.as_mut() {
                        *pixel_format = PIXEL_FORMAT;
                    }
                }
            }

            // SAFETY: null check relevant parameters and return null if any of them are null
            unsafe extern "C" fn get_color_channel_data_at_extern(
                opaque: *mut c_void,
                xpos: usize,
                ypos: usize,
                _: usize,
                _: usize,
                row_offset: *mut usize,
            ) -> *const c_void {
                unsafe {
                    let Some(this) = (opaque as *mut Self).as_ref() else {
                        return ptr::null_mut();
                    };
                    let Some(row_offset) = row_offset.as_mut() else {
                        return ptr::null_mut();
                    };
                    let row_mem_width = this.size.width + this.stride_to_next_row;
                    *row_offset = row_mem_width * mem::size_of::<Pixel24>();
                    this.bytes[(xpos + (ypos * row_mem_width)) * mem::size_of::<Pixel24>()..]
                        .as_ref() as *const _ as _
                }
            }

            // SAFETY: null check relevant parameters and return early if any of them are null
            unsafe extern "C" fn get_extra_channel_pixel_format_extern(
                _: *mut c_void,
                _: usize,
                pixel_format: *mut JxlPixelFormat,
            ) {
                unsafe {
                    if let Some(pixel_format) = pixel_format.as_mut() {
                        *pixel_format = NO_EXTRA_CHANNELS_FORMAT;
                    }
                }
            }

            // SAFETY: this function is a no-op
            unsafe extern "C" fn get_extra_channel_data_at_extern(
                _: *mut c_void,
                _: usize,
                _: usize,
                _: usize,
                _: usize,
                _: usize,
                _: *mut usize,
            ) -> *const c_void {
                ptr::null()
            }

            // SAFETY: this function is a no-op
            unsafe extern "C" fn release_buffer_extern(_: *mut c_void, _: *const c_void) {}

            fn as_jxl(&mut self) -> JxlChunkedFrameInputSource {
                JxlChunkedFrameInputSource {
                    opaque: self as *mut _ as _,
                    get_color_channel_data_at: Some(Self::get_color_channel_data_at_extern),
                    get_color_channels_pixel_format: Some(
                        Self::get_color_channels_pixel_format_extern,
                    ),
                    get_extra_channel_data_at: Some(Self::get_extra_channel_data_at_extern),
                    get_extra_channel_pixel_format: Some(
                        Self::get_extra_channel_pixel_format_extern,
                    ),
                    release_buffer: Some(Self::release_buffer_extern),
                }
            }
        }

        #[derive(Default)]
        struct OutputBuffer {
            buf: Vec<u8>,
            position: usize,
        }

        impl OutputBuffer {
            // SAFETY: null check parameters and return null if any of them are null, then call
            // into the safe version of this function
            unsafe extern "C" fn get_buffer_extern(
                opaque: *mut c_void,
                size: *mut usize,
            ) -> *mut c_void {
                unsafe {
                    let Some(this) = (opaque as *mut Self).as_mut() else {
                        return ptr::null_mut();
                    };
                    let Some(size) = size.as_mut() else {
                        return ptr::null_mut();
                    };

                    this.get_buffer(size) as *mut _ as _
                }
            }

            fn get_buffer(&mut self, size: &mut usize) -> &mut [MaybeUninit<u8>] {
                let requested_size = *size;
                let finalized_position = self.buf.len();
                let unfinalized_written_space = self.position - finalized_position;
                let free_space = self.buf.capacity() - self.position;
                if free_space < requested_size {
                    let reserve_amt = unfinalized_written_space + requested_size;
                    self.buf.reserve_exact(reserve_amt);
                }

                *size = self.buf.capacity() - self.position;
                self.buf.spare_capacity_mut()[self.position..].as_mut()
            }

            // SAFETY: null check parameters and return null if any of them are null, then call
            // into the safe version of this function
            unsafe extern "C" fn release_buffer_extern(opaque: *mut c_void, written_bytes: usize) {
                unsafe {
                    let Some(this) = (opaque as *mut Self).as_mut() else {
                        return;
                    };
                    this.release_buffer(written_bytes)
                }
            }

            fn release_buffer(&mut self, written_bytes: usize) {
                self.position += written_bytes;
            }

            // SAFETY: null check parameters and return null if any of them are null, then call
            // into the safe version of this function
            unsafe extern "C" fn seek_extern(opaque: *mut c_void, position: u64) {
                unsafe {
                    let Some(this) = (opaque as *mut Self).as_mut() else {
                        return;
                    };
                    this.seek(position as _)
                }
            }

            fn seek(&mut self, position: usize) {
                if position >= self.buf.capacity() {
                    self.buf.reserve_exact(position - self.buf.len());
                }
                self.position = position;
            }

            // SAFETY: null check parameters and return null if any of them are null, then call
            // into the safe version of this function
            unsafe extern "C" fn set_finalized_position_extern(
                opaque: *mut c_void,
                finalized_position: u64,
            ) {
                unsafe {
                    let Some(this) = (opaque as *mut Self).as_mut() else {
                        return;
                    };
                    this.set_finalized_position(finalized_position as _)
                }
            }

            fn set_finalized_position(&mut self, finalized_position: usize) {
                if finalized_position > self.buf.capacity() {
                    let reserve_amt = finalized_position - self.buf.len();
                    self.buf.reserve(reserve_amt);
                }
                // SAFETY: we explicitly reserve at least the amount we need just before this call
                unsafe { self.buf.set_len(finalized_position) }
            }

            fn as_jxl(&mut self) -> JxlEncoderOutputProcessor {
                JxlEncoderOutputProcessor {
                    opaque: self as *mut OutputBuffer as *mut _,
                    get_buffer: Some(Self::get_buffer_extern),
                    release_buffer: Some(Self::release_buffer_extern),
                    seek: Some(Self::seek_extern),
                    set_finalized_position: Some(Self::set_finalized_position_extern),
                }
            }
        }

        pub struct Compressor {
            encoder: AsyncMutex<&'static mut JxlEncoder>,
            par_runner_opaque: AsyncMutex<&'static mut c_void>,
        }
        // SAFETY: *mut JxlEncoder can be sent between threads without issue
        unsafe impl Send for Compressor {}
        // SAFETY: we only access the *mut JxlEncoder via a mutex to synchronize between threads
        unsafe impl Sync for Compressor {}

        impl Drop for Compressor {
            fn drop(&mut self) {
                // SAFETY:
                //   - enc is guaranteed to be non-null as it is a reference
                unsafe { JxlEncoderDestroy(*self.encoder.get_mut()) }
                // SAFETY:
                //   - par_runner_opaque is guaranteed to be non-null as it is a reference
                unsafe { JxlResizableParallelRunnerDestroy(*self.par_runner_opaque.get_mut()) }
            }
        }

        impl Compressor {
            pub fn new() -> Result<Self> {
                // SAFETY:
                //   - JxlEncoderCreate can take a null pointer to skip using a custom memory
                //   manager
                //   - jxl_call! checks the return value of JxlEncoderCreate and bails if it
                //   fails
                let encoder = jxl_call!(JxlEncoderCreate, ptr::null())?;

                // SAFETY:
                //   - JxlResizableParallelRunnerCreate can take a null pointer to skip using a
                //   custom memory manager
                //   - jxl_call! checks the return value of JxlResizableParallelRunnerCreate and
                //   bails if it fails
                let par_runner_opaque = jxl_call!(JxlResizableParallelRunnerCreate, ptr::null())?;

                // SAFETY:
                //   - encoder is guaranteed to be non-null as it is a reference
                //   - par_runner_opaque is guaranteed to be non-null as it is a reference
                //   - jxl_call! checks the return value of JxlEncoderSetParallelRunner and bails
                //   if it fails
                jxl_call!(
                    JxlEncoderSetParallelRunner,
                    encoder,
                    Some(JxlResizableParallelRunner),
                    par_runner_opaque
                )?;

                Ok(Compressor {
                    encoder: AsyncMutex::new(encoder),
                    par_runner_opaque: AsyncMutex::new(par_runner_opaque),
                })
            }

            pub async fn compress(&self, raw_data: ArrayView2<'_, Pixel24>) -> Result<Vec<u8>> {
                fn do_compress(
                    enc: &mut JxlEncoder,
                    par_runner_opaque: &mut c_void,
                    raw_data: ArrayView2<'_, Pixel24>,
                ) -> Result<Vec<u8>> {
                    let width = raw_data.len_of(axis::X);
                    let height = raw_data.len_of(axis::Y);
                    let col_stride = raw_data.stride_of(axis::X);
                    let row_stride = raw_data.stride_of(axis::Y);
                    if width > 1 {
                        ensure!(
                            col_stride == 1,
                            "Elements in each column are not-adjacent, stride is {}",
                            col_stride
                        );
                    } else {
                        ensure!(width == 1, "Width <= 0");
                    }

                    let stride_to_next_row = if height > 1 {
                        ensure!(
                            row_stride > 0,
                            "Rows are traversed in reverse order, stride is {}",
                            row_stride
                        );
                        row_stride as usize - width
                    } else {
                        ensure!(height == 1, "Height <= 0");
                        0
                    };

                    // SAFETY: We calculate the len of the slice as a function of the width,
                    // height, stride, and size of an element, so the slice is guaranteed to be
                    // valid
                    let bytes = unsafe {
                        let ptr = raw_data.as_ptr() as *const u8;
                        let len = (width + stride_to_next_row) * height * mem::size_of::<Pixel24>();
                        slice::from_raw_parts(ptr, len)
                    };

                    // SAFETY: JxlResizableParallelRunnerSuggestThreads is only marked unsafe at it
                    // is an external C++ function. Nothing about the function is inherently unsafe
                    let num_threads = unsafe {
                        JxlResizableParallelRunnerSuggestThreads(width as _, height as _)
                    };
                    // SAFETY:
                    //   - par_runner_opaque is guaranteed to be non-null as it is a reference
                    unsafe {
                        JxlResizableParallelRunnerSetThreads(
                            par_runner_opaque,
                            num_threads.max(1) as _,
                        )
                    };

                    let mut output_buffer = OutputBuffer::default();
                    // SAFETY:
                    //   - enc is guaranteed to be non-null as it is a reference
                    //   - output_buffer.as_jxl() produces a non-null reference which is guaranteed
                    //   to exist for the life of the OutputBuffer
                    //   - jxl_call! checks the return value of JxlEncoderSetOutputProcessor and
                    //   bails if it fails
                    jxl_call!(JxlEncoderSetOutputProcessor, enc, output_buffer.as_jxl())?;

                    // SAFETY: we initialize JxlBasicInfo with a call to JxlEncoderInitBasicInfo,
                    // which does not fail when passed a valid pointer
                    let mut bi = unsafe {
                        let mut uninit: MaybeUninit<JxlBasicInfo> = MaybeUninit::uninit();
                        JxlEncoderInitBasicInfo(uninit.as_mut_ptr());
                        uninit.assume_init()
                    };

                    bi.xsize = width as _;
                    bi.ysize = height as _;
                    bi.bits_per_sample = parameters::BITS_PER_SAMPLE;
                    bi.exponent_bits_per_sample = parameters::EXPONENT_BITS_PER_SAMPLE;
                    bi.uses_original_profile = parameters::USES_ORIGINAL_PROFILE;
                    // SAFETY:
                    //   - enc is guaranteed to be non-null as it is a reference
                    //   - jxl_call! checks the return value of JxlEncoderSetBasicInfo and bails if
                    //   it fails
                    jxl_call!(JxlEncoderSetBasicInfo, enc, &bi)?;

                    // SAFETY:
                    //   - enc is guaranteed to be non-null as it is a reference
                    //   - JxlEncoderFrameSettingsCreate can take a null source value
                    //   - jxl_call! checks the return value of JxlEncoderFrameSettingsCreate and
                    //   bails if it fails
                    let frame_settings =
                        jxl_call!(JxlEncoderFrameSettingsCreate, enc, ptr::null())?;

                    // SAFETY:
                    //   - frame_settings is null-checked above
                    //   - jxl_call! checks the return value of JxlEncoderFrameSettingsSetOption
                    //   and bails if it fails
                    jxl_call!(
                        JxlEncoderFrameSettingsSetOption,
                        frame_settings,
                        JxlEncoderFrameSettingId::JXL_ENC_FRAME_SETTING_EFFORT,
                        parameters::ENCODING_EFFORT,
                    )?;
                    // SAFETY: See above
                    jxl_call!(
                        JxlEncoderFrameSettingsSetOption,
                        frame_settings,
                        JxlEncoderFrameSettingId::JXL_ENC_FRAME_SETTING_DECODING_SPEED,
                        parameters::DECODING_SPEED,
                    )?;
                    // SAFETY: See above
                    jxl_call!(
                        JxlEncoderFrameSettingsSetOption,
                        frame_settings,
                        JxlEncoderFrameSettingId::JXL_ENC_FRAME_SETTING_BUFFERING,
                        parameters::BUFFERING
                    )?;
                    // SAFETY: See above
                    jxl_call!(
                        JxlEncoderFrameSettingsSetOption,
                        frame_settings,
                        JxlEncoderFrameSettingId::JXL_ENC_FRAME_SETTING_COLOR_TRANSFORM,
                        parameters::COLOR_TRANSFORM
                    )?;
                    // SAFETY: See above
                    jxl_call!(
                        JxlEncoderFrameSettingsSetOption,
                        frame_settings,
                        JxlEncoderFrameSettingId::JXL_ENC_FRAME_SETTING_RESPONSIVE,
                        parameters::RESPONSIVE,
                    )?;
                    // SAFETY: See above
                    jxl_call!(
                        JxlEncoderSetFrameDistance,
                        frame_settings,
                        parameters::DISTANCE
                    )?;

                    let mut input_source =
                        InputSource::new(bytes, Size::new(width, height), stride_to_next_row)?;
                    // SAFETY:
                    //   - enc is guaranteed to be non-null as it is a reference
                    //   - input_source.as_jxl() produces a non-null reference which is guaranteed
                    //   to exist for the life of the InputSource
                    //   - jxl_call! checks the return value of JxlEncoderAddChunkedFrame and bails
                    //   if it fails
                    jxl_call!(
                        JxlEncoderAddChunkedFrame,
                        frame_settings,
                        1, /* is last frame */
                        input_source.as_jxl()
                    )?;
                    // SAFETY:
                    //   - enc is guaranteed to be non-null as it is a reference
                    //   - jxl_call! checks the return value of JxlEncoderReset and bails if it
                    //   fails
                    jxl_call!(JxlEncoderFlushInput, enc)?;

                    // SAFETY:
                    //   - enc is guaranteed to be non-null as it is a reference
                    unsafe { JxlEncoderReset(enc) };
                    output_buffer.buf.shrink_to_fit();
                    Ok(output_buffer.buf)
                }
                task::block_in_place(move || {
                    do_compress(
                        *self.encoder.blocking_lock(),
                        *self.par_runner_opaque.blocking_lock(),
                        raw_data,
                    )
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
        let task: JoinHandle<Result<()>> = task::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    Some(command) = self.state.command_rx.recv() => {
                        let Command::Reconfigure((new_id, area)) = command;
                        debug!(old_encoder=%self.id, new_encoder=%new_id, old_area=?self.state.area, new_area=?area, "Reconfigure");
                        self.id = new_id;
                        self.state.area = area;
                    },
                    Some(frame) = self.state.frame_rx.recv() => {
                        let relevant = frame
                            .buffers
                            .iter()
                            .filter_map(|buffer| {
                                self.state
                                    .area
                                    .intersection(&buffer.area().to_box2d())
                                    .map(|intersection| (buffer, intersection))
                            })
                            .collect::<Vec<_>>();

                        if relevant.is_empty() {
                            self.state
                                .mm_tx
                                .send(mm_s2c::Message::Terminal(mm::EmptyFrameTerminal {
                                    frame: frame.id as u64,
                                    encoder: self.id as u32,
                                    display_config: frame.display_config as u32,
                                    display: frame.display as u32,
                                }))
                            .await?;
                            trace!(
                                display=%frame.display,
                                dc=%frame.display_config,
                                frame=%frame.id,
                                encoder=%self.id,
                                "Dispatched empty frame"
                            );
                        } else {
                            let mut encodings = FuturesUnordered::new();
                            for (buffer, intersection) in &relevant {
                                encodings.push(async {
                                    let buffer_relative_area = intersection
                                        .to_i64()
                                        .translate(-buffer.origin.to_vector().to_i64())
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
                                    TimeInCompression::observe((end - start).as_secs_f64() * 1_000.0);
                                    PixelsCompressed::inc_by(size.area() as f64);

                                    let nbytes_produced = data.len();
                                    let nbytes_consumed = size.area() * mem::size_of::<Pixel24>();

                                    let ratio = 100_f64 * (nbytes_produced as f64 / nbytes_consumed as f64);
                                    CompressionRatio::observe(ratio);

                                    Ok::<_, anyhow::Error>((data, enc_relative_area))
                                });
                            }

                            let mut sequence = 0;
                            while let Some(res) = encodings.next().await {
                                let terminal = sequence == relevant.len() - 1;
                                match res {
                                    Ok((data, enc_relative_area)) => {
                                        let frag = mm::FrameFragment {
                                            frame: frame.id as u64,
                                            encoder: self.id as u32,
                                            display_config: frame.display_config as u32,
                                            display: frame.display as u32,
                                            sequence: sequence as u32,
                                            terminal,
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

                                        trace!(
                                            display=%frame.display,
                                            dc=%frame.display_config,
                                            frame=%frame.id,
                                            encoder=%self.id,
                                            %terminal,
                                            sequence,
                                            total_fragments=?relevant.len(),
                                            "Dispatched frame fragment"
                                        );

                                        sequence += 1;
                                    },
                                    Err(error) => {
                                        warn!(
                                            display=%frame.display,
                                            dc=%frame.display_config,
                                            frame=%frame.id,
                                            encoder=%self.id,
                                            %terminal,
                                            sequence,
                                            total_fragments=?relevant.len(),
                                            %error,
                                            "failed encoding"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    else => bail!("[encoder {}] exhausted", self.id),
                }
            }
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
