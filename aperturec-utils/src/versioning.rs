use ouroboros::self_referencing;
use std::fmt::Display;
use std::io;
use thiserror::Error;
use version_compare::Version;

#[derive(Debug, Error)]
pub enum KernelError {
    #[error("uname failed")]
    Uname(#[from] io::Error),
    #[error("kernel version string parse failed")]
    KernelParse,
    #[error("version string parse failed")]
    Parse,
}

#[self_referencing]
pub struct Kernel {
    version_str: String,
    #[borrows(version_str)]
    #[covariant]
    version: Version<'this>,
}

impl Kernel {
    pub fn meets_or_exceeds(&self, other: impl AsRef<str>) -> Result<bool, KernelError> {
        let other_version = Version::from(other.as_ref()).ok_or(KernelError::Parse)?;
        Ok(self.with_version(|v| *v >= other_version))
    }
}

impl Display for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.with_version(|v| v.fmt(f))
    }
}

impl TryFrom<String> for Kernel {
    type Error = KernelError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Kernel::try_new(value, |version_str| {
            Version::from(version_str).ok_or(KernelError::KernelParse)
        })
    }
}

impl TryFrom<&str> for Kernel {
    type Error = KernelError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Kernel::try_from(value.to_string())
    }
}

pub fn running_kernel() -> Result<Kernel, KernelError> {
    let info = uname::uname()?;
    Kernel::try_from(info.release)
}
