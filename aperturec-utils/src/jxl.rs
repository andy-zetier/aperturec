use anyhow::{Result, anyhow};
use jxl_sys::*;

pub mod parameters {
    /// 8-bit (u8) color samples
    pub const BITS_PER_SAMPLE: u32 = 8;

    /// integer/non-float color samples
    pub const EXPONENT_BITS_PER_SAMPLE: u32 = 0;

    /// use original colors (RGB)
    pub const USES_ORIGINAL_PROFILE: i32 = 1;

    /// Lightning effort (fastest)
    pub const ENCODING_EFFORT: i64 = 1;

    /// Fastest decode speed
    pub const DECODING_SPEED: i64 = 4;

    /// Most aggressive buffering
    pub const BUFFERING: i64 = 2;

    /// use original colors (RGB) internally
    pub const COLOR_TRANSFORM: i64 = 1;

    /// Distance (quality) for visual losslessness
    pub const DISTANCE: f32 = 1.0;
}

pub const PIXEL_FORMAT: JxlPixelFormat = JxlPixelFormat {
    num_channels: 3,
    data_type: JxlDataType::JXL_TYPE_UINT8,
    endianness: JxlEndianness::JXL_NATIVE_ENDIAN,
    align: 0,
};

pub const NO_EXTRA_CHANNELS_FORMAT: JxlPixelFormat = JxlPixelFormat {
    num_channels: 0,
    data_type: JxlDataType::JXL_TYPE_UINT8,
    endianness: JxlEndianness::JXL_NATIVE_ENDIAN,
    align: 0,
};

pub trait JxlReturn<R> {
    fn ok_with_name(self, name: &str) -> Result<R>;
}

impl JxlReturn<()> for JxlEncoderStatus {
    fn ok_with_name(self, name: &str) -> Result<()> {
        if self == JxlEncoderStatus::JXL_ENC_SUCCESS {
            Ok(())
        } else {
            Err(anyhow!("{} failed with status: {:?}", name, self))
        }
    }
}

impl JxlReturn<()> for JxlDecoderStatus {
    fn ok_with_name(self, name: &str) -> Result<()> {
        if self == JxlDecoderStatus::JXL_DEC_SUCCESS {
            Ok(())
        } else {
            Err(anyhow!("{} failed with status: {:?}", name, self))
        }
    }
}

impl<'r, T> JxlReturn<&'r mut T> for *mut T {
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn ok_with_name(self, name: &str) -> Result<&'r mut T> {
        // SAFETY: check self for null and return error if it is
        unsafe {
            if let Some(r) = self.as_mut() {
                Ok(r)
            } else {
                Err(anyhow!("{} failed", name))
            }
        }
    }
}

#[macro_export]
macro_rules! jxl_call {
    ( $fn:path, $( $arg:expr ),* $(,)* ) => {{
        // SAFETY: caller must uphold any C preconditions
        let status = unsafe { $fn( $( $arg ),* ) };
        status.ok_with_name(std::stringify!($fn))
    }};
}
pub use jxl_call;
