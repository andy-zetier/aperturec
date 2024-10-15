# ApertureC Protocol (ACP)

This repository houses the definitions and Rust language bindings for the
ApertureC Protocol (ACP) Further information about the protocol can be found
in the ACP Specification.

## Overview

- `proto/`: Protobufs Definitions of all PDUs in ACP
- `src/`: Contains tests and `includes!` to include the auto generated code
- `build.rs`: Build script which auto-generates the Rust bindings for the PDUs

## Usage

You should be able to use the ACP PDUs simply by specifying this crate as a
Cargo dependency.
