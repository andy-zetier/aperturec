use std::env;
use std::process::Command;

fn get_git_version() -> Result<String, String> {
    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").map_err(|e| format!("get manifest directory: {e}"))?;

    Command::new("git")
        .arg("-C")
        .arg(manifest_dir)
        .arg("describe")
        .arg("--always")
        .arg("--dirty")
        .output()
        .map_err(|e| format!("spawning `git` caused error: {e}"))
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout)
                    .map_err(|e| format!("parsing git output caused error: {e}"))
                    .map(|s| s.trim().to_string())
            } else {
                Err(format!(
                    "git exited with status: {:?}",
                    output.status.code(),
                ))
            }
        })
}

fn main() {
    let cargo_pkg_version = env!("CARGO_PKG_VERSION");
    let new_version = match get_git_version() {
        Ok(version) => format!("{cargo_pkg_version}+{version}"),
        Err(e) => {
            println!("cargo:warning=Failed to get git version with error: {e}");
            println!("cargo:warning=Using CARGO_PKG_VERSION as version");
            cargo_pkg_version.to_string()
        }
    };
    println!("cargo:rustc-env=CARGO_PKG_VERSION={new_version}");
}
