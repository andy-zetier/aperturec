use directories::ProjectDirs;
use std::env;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::LazyLock;

const QUALIFIER: &str = "com";
const ORGANIZATION: &str = "zetier";
const APPLICATION: &str = "aperturec";

const LOG_DIR_NAME: &str = "logs";

const PROJECT_DIRS: LazyLock<ProjectDirs> =
    LazyLock::new(|| ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION).expect("ProjectDirs"));

pub fn log_dir() -> PathBuf {
    let mut pb = PROJECT_DIRS.data_dir().to_path_buf();
    pb.push(LOG_DIR_NAME);
    pb
}

pub fn log_basename(dir: &Path) -> PathBuf {
    let mut pb = dir.to_path_buf();
    let exe_path = env::current_exe().expect("current_exe");
    let exe_file_name = exe_path
        .file_stem()
        .expect("file_stem")
        .to_str()
        .expect("to_str");
    pb.push(format!("{}.{}", exe_file_name, process::id()));
    pb
}
