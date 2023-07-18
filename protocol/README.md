# ApertureC Protocol (ACP)

This repository houses the definitions and Rust language bindings for the
ApertureC Protocol (ACP) Further information about the protocol can be found
in the ACP Specification.

## Overview

- `asn1/`: ASN.1 Definitions of all PDUs in ACP
- `src/`: Contains only a `lib.rs` which `include!`'s auto-generated Rust
    bindings for the PDUs
- `tests/`: Tests which are executable via `cargo test` Additionally this is
    probably the best place to see how to use the PDUs
- `build.rs`: Build script which auto-generates the Rust bindings for the PDUs
    via [asn1rs](https://crates.io/crates/asn1rs).


## Usage

You should be able to use the ACP PDUs simply by specifying this crate as a
Cargo dependency.
