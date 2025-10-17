#[cfg(feature = "ffi-lib")]
fn main() {
    use std::env;
    use std::path::PathBuf;

    let crate_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset");

    let mut out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR unset"));
    out_path.push("aperturec_client.h");

    let config = cbindgen::Config::from_root_or_default(&crate_dir);

    let bindings = cbindgen::Builder::new()
        .with_config(config)
        .with_crate(crate_dir)
        .generate()
        .expect("Unable to generate bindings");
    bindings.write_to_file(out_path);
}

#[cfg(not(feature = "ffi-lib"))]
fn main() {}
