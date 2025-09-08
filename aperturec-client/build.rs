#[cfg(feature = "ffi-lib")]
fn main() {
    use std::env;
    use std::path::PathBuf;

    let crate_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset");

    let mut out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR unset"));
    out_path.push("aperturec_client.h");

    cbindgen::Builder::new()
        .with_crate(crate_dir)
        .generate()
        .expect("Unable to generate bindings")
        .write_to_file(out_path);
}

#[cfg(not(feature = "ffi-lib"))]
fn main() {}
