use asn1rs::converter::Converter;
use std::env;

fn main() {
    let mut converter = Converter::default();

    std::fs::read_dir("asn1")
        .unwrap()
        .filter_map(|de_res| de_res.ok())
        .filter(|de| de.path().extension().is_some_and(|ext| ext == "asn1"))
        .map(|de| de.path())
        .for_each(|path| {
            let path_display = path.display();
            println!("cargo:rerun-if-changed={}", path_display);
            if let Err(e) = converter.load_file(&path) {
                panic!("Loading of {} failed: {:?}", path_display, e)
            }
        });

    converter
        .to_rust(env::var("OUT_DIR").unwrap(), |_| {})
        .unwrap();
}
