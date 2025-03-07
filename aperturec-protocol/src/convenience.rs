use crate::common;

impl TryFrom<&str> for common::SemVer {
    type Error = String;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        let sv = semver::Version::parse(s).map_err(|e| format!("Invalid version: {}", e))?;
        Ok(sv.into())
    }
}

impl common::SemVer {
    pub fn from_cargo() -> Result<Self, String> {
        env!("CARGO_PKG_VERSION").try_into()
    }
}

impl From<semver::Version> for common::SemVer {
    fn from(sv: semver::Version) -> Self {
        Self {
            major: sv.major,
            minor: sv.minor,
            patch: sv.patch,
            pre_release: sv.pre.to_string(),
            build_metadata: sv.build.to_string(),
        }
    }
}

impl TryFrom<common::SemVer> for semver::Version {
    type Error = String;

    fn try_from(sv: common::SemVer) -> Result<Self, String> {
        let mut v = semver::Version::new(sv.major, sv.minor, sv.patch);
        v.pre = sv.pre_release.parse().unwrap_or_default();
        v.build = sv.build_metadata.parse().unwrap_or_default();
        Ok(v)
    }
}
