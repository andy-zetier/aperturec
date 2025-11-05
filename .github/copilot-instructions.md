# ApertureC Development Instructions

ApertureC is a Rust workspace containing a QUIC-based remote desktop server and GTK4 client application. The project provides low-latency screen sharing and control capabilities with advanced graphics optimization.

Always reference these instructions first and fallback to search or bash commands only when you encounter unexpected information that does not match the info here.

## Working Effectively

### System Dependencies
Install required system dependencies before building:
```bash
sudo apt update && sudo apt install -y libgtk-4-dev libglib2.0-dev pkg-config protobuf-compiler build-essential
```

### Build Commands
- **CRITICAL**: Always install system dependencies first (see above)
- Bootstrap and build the workspace:
  - `cargo check` - Initial compilation check and dependency download (takes ~6 minutes)
  - `cargo build --release` - Full release build (takes ~20-25 minutes). NEVER CANCEL. Set timeout to 45+ minutes.
  - `cargo build --release -p aperturec-server` - Build only the server (takes ~15-20 minutes). NEVER CANCEL.
  - `cargo build --release -p aperturec-client-gtk4` - Build only the client (takes ~15-20 minutes). NEVER CANCEL.

### Test Commands
- `cargo test --workspace` - Run all tests (takes ~15-20 minutes). NEVER CANCEL. Set timeout to 30+ minutes.
- `cargo test -p aperturec-channel` - Test network layer
- `cargo test -p aperturec-graphics` - Test graphics processing
- `cargo test -p aperturec-state-machine` - Test state machine library

### Development Commands
- `cargo fmt --all` - Format all code (required before commits)
- `cargo clippy --workspace --all-targets` - Lint all code (required before commits)
- `cargo deny check` - Check dependencies for security/license issues
- `cargo check` - Fast compilation check during development

### Running Applications
Both applications require X11 environment. The client requires a running server to connect to.

#### Server
```bash
# Basic server startup (requires build first)
cargo run -p aperturec-server -- --help

# Example server with specific configuration
cargo run -p aperturec-server -- \
  --bind-address=127.0.0.1:46452 \
  --screen-config=1920x1080 \
  "glxgears -fullscreen"
```

#### Client
```bash
# Basic client startup (requires build first)
cargo run -p aperturec-client-gtk4 -- --help

# Connect to local server
cargo run -p aperturec-client-gtk4 -- connect 127.0.0.1:46452
```

## Validation

### Manual Testing Scenarios
Always test these scenarios after making changes:

1. **Build Validation**: 
   - Run `cargo check` and ensure it completes successfully
   - Run `cargo build --release` and verify both server and client binaries are created in `target/release/`

2. **Test Validation**:
   - Run `cargo test --workspace` and ensure all tests pass
   - Pay special attention to network tests in `aperturec-channel`

3. **Application Functionality** (if X11 environment available):
   - Start server with test application: `cargo run -p aperturec-server -- "xterm"`
   - Connect with client: `cargo run -p aperturec-client-gtk4 -- connect 127.0.0.1:46452`
   - Verify you can see and interact with the remote application

4. **Code Quality**:
   - Always run `cargo fmt --all` before committing
   - Always run `cargo clippy --workspace --all-targets` before committing
   - Run `cargo deny check` to verify dependency compliance

### CI/CD Validation
The CI pipeline (.github/workflows/ci.yml) will fail if:
- Code is not formatted (`cargo fmt --all` required)
- Clippy warnings exist (`cargo clippy` required)
- Tests fail (`cargo test --workspace` required)
- Dependencies have security/license issues (`cargo deny check` required)

## Project Structure

### Key Workspace Crates
- `aperturec-server/` - QUIC server application with X11 capture
- `aperturec-client-gtk4/` - GTK4 client application
- `aperturec-client/` - Core client library
- `aperturec-channel/` - QUIC network layer (s2n-quic based)
- `aperturec-graphics/` - Graphics processing and optimization
- `aperturec-protocol/` - Protocol buffer definitions and generated code
- `aperturec-state-machine/` - Generic compile-time state machine library
- `aperturec-metrics/` - Application metrics collection
- `aperturec-utils/` - Shared utilities

### Important Files
- `Cargo.toml` - Workspace configuration
- `deny.toml` - Dependency policy configuration
- `scripts/test.py` - Integration testing script with Xvfb
- `ci/` - Docker build environments and CI scripts
- `.github/workflows/ci.yml` - Main CI pipeline

### Protocol Buffers
The `aperturec-protocol/` crate auto-generates Rust code from `.proto` files in `aperturec-protocol/proto/`. Changes to protocol definitions require rebuilding the protocol crate.

## Common Tasks

### Repository Structure Overview
```
/home/runner/work/aperturec/aperturec/
├── Cargo.toml                 # Workspace root
├── Cargo.lock                 # Dependency lock file
├── deny.toml                  # Dependency policy
├── aperturec-server/          # Server application
├── aperturec-client-gtk4/     # GTK4 client application
├── aperturec-client/          # Client library
├── aperturec-channel/         # QUIC networking
├── aperturec-graphics/        # Graphics processing
├── aperturec-protocol/        # Protocol definitions
├── aperturec-state-machine/   # State machine library
├── aperturec-metrics/         # Metrics collection
├── aperturec-utils/           # Shared utilities
├── scripts/test.py            # Integration testing
├── ci/                        # CI environments
└── .github/                   # GitHub Actions workflows
```

### Environment Requirements
- **Rust**: 1.89.0+ (2024 edition required)
- **System**: Linux with X11 support
- **Dependencies**: GTK4, GLib, protobuf-compiler, pkg-config
- **Optional**: Xvfb for headless testing
- **Network**: QUIC networking requires modern Linux (kernel 4.18+)

### Performance Notes
- Release builds enable LTO and optimizations, significantly increasing build time
- Server includes X11 capture and graphics processing - requires display server
- Client uses GTK4 for UI - requires GUI environment
- Network layer uses AWS s2n-quic for high-performance QUIC
- Graphics processing uses hardware acceleration when available

### Troubleshooting
- **Build fails with glib-sys errors**: Install GTK4 development packages
- **Missing protobuf compiler**: Install `protobuf-compiler` package
- **X11 connection errors**: Server needs X11 display (use Xvfb for headless)
- **QUIC connection issues**: Check firewall and kernel version (4.18+ required)
- **Long build times**: This is normal for release builds with LTO enabled

## Build Time Expectations
Based on testing on GitHub Actions runners:
- **cargo check**: ~6 minutes (first run with dependency download)
- **cargo build --release**: ~20-25 minutes (NEVER CANCEL - set 45+ minute timeout)
- **cargo test --workspace**: ~15-20 minutes (NEVER CANCEL - set 30+ minute timeout)
- **cargo clippy**: ~8-12 minutes
- Subsequent builds are faster due to incremental compilation

**NEVER CANCEL BUILDS OR TESTS** - Rust release builds with LTO take significant time but are necessary for performance.
