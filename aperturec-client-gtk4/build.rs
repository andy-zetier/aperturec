//! Build script for packaging GTK resources and embedding a git-derived version.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Return the current `git describe` output for the manifest directory.
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

/// Compile the GTK resource bundle if the XML manifest is present.
fn compile_resources(resources_dir: &Path, out_dir: &Path) {
    let gresource_xml = resources_dir.join("aperturec-client.gresource.xml");
    if !gresource_xml.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={}", gresource_xml.display());

    let target = out_dir.join("aperturec-client.gresource");
    let status = Command::new("glib-compile-resources")
        .arg("--target")
        .arg(&target)
        .arg("--sourcedir")
        .arg(resources_dir)
        .arg(&gresource_xml)
        .status()
        .unwrap_or_else(|error| {
            panic!("failed to run glib-compile-resources for {gresource_xml:?}: {error}")
        });
    if !status.success() {
        panic!("glib-compile-resources failed for {gresource_xml:?}");
    }
}

/// Emit `rerun-if-changed` for all files under the resources directory.
fn watch_resources(dir: &Path) {
    if !dir.exists() {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => panic!("failed to read resource dir {dir:?}: {error}"),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => panic!("failed to read resource dir entry: {error}"),
        };
        let path = entry.path();
        if path.is_dir() {
            watch_resources(&path);
        } else if path.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// Entry point for the build script.
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

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("out dir"));
    let resources_dir = manifest_dir.join("resources");

    compile_resources(&resources_dir, &out_dir);
    watch_resources(&resources_dir);
}
