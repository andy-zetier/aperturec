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

## ALPN Versioning

The protocol defines ALPN (Application-Layer Protocol Negotiation) identifiers used
in TLS handshakes for QUIC connections:

- `MAGIC`: Current ALPN identifier using only the **major version** (e.g., `ApertureC-1`)
- `LEGACY_ALPN`: Legacy ALPN identifier for backward compatibility

This versioning strategy ensures that:
- Minor and patch version updates don't break compatibility
- Clients and servers can negotiate connections across different minor/patch versions
- Breaking changes can be signaled via major version increments
