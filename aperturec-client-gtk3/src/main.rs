fn main() {
    aperturec_client::run(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
        .expect("client exited with error")
}
