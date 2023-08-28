# ApertureC Protocol (ACP)

This repository houses the definitions and Rust language bindings for the
ApertureC Protocol (ACP) Further information about the protocol can be found
in the ACP Specification.

## Overview

- `asn1/`: ASN.1 Definitions of all PDUs in ACP
- `src/`: Contains tests and `includes!` to include the auto generated code
- `build.rs`: Build script which auto-generates the Rust bindings for the PDUs
    via [asn1rs](https://crates.io/crates/asn1rs). Additionally builds and links
    [asn1c](https://github.com/vlm/asn1c) for round-trip Rust-to-C-and-back
    testing


## Usage

You should be able to use the ACP PDUs simply by specifying this crate as a
Cargo dependency.
