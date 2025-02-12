pub mod args;
#[macro_use]
pub mod log;
pub mod paths;

#[cfg(target_os = "linux")]
pub mod versioning;

pub const fn build_id() -> &'static str {
    if let Some(iid) = option_env!("CI_PIPELINE_IID") {
        iid
    } else {
        git_version::git_version!(prefix = "git:")
    }
}
