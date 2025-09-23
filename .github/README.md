# ApertureC CI/CD Pipeline

This directory contains GitHub Actions workflows, composite actions, and
configuration files that implement ApertureC's continuous integration system.

## Architecture Overview

The CI uses a **two-phase approach**:
1. **Image Creation**: Build Docker images with all dependencies pre-installed
2. **CI Execution**: Run jobs using the pre-built images for consistency and speed

This design ensures all jobs use identical environments and eliminates
dependency setup time during CI runs.

## Workflows

### `ci.yml` - Main CI Pipeline
The primary workflow that runs on pushes and pull requests. It:
- Builds and tests code across Linux (amd64/arm64), macOS, and Windows
- Performs code quality checks (format, lint, audit)
- Creates release packages (deb, rpm, dmg, msi)
- Generates documentation

### `create-images.yml` - Reusable Image Builder
A reusable workflow that builds Docker images and returns a digest index:
- Takes an image configuration JSON as input
- Builds images for multiple OS/architecture combinations
- Returns SHA256 digests indexed by `"{target}-{os}-{arch}"` keys
- Used by the main CI to ensure consistent environments

## Image Strategy

1. **Pre-build Phase**: `create-images.yml` builds Docker images containing:
   - Rust toolchain and components
   - System dependencies (GTK, protobuf, etc.)
   - Build tools and utilities

2. **Indexing**: Images are tagged with SHA256 digests and indexed by:
   ```
   rust-image-ubuntu-24-amd64 -> sha256:abc123...
   client-image-ubuntu-24-arm64 -> sha256:def456...
   ```

3. **Usage**: CI jobs reference images via digest lookup:
   ```yaml
   container:
     image: "${{ container-repo }}@${{ digests['rust-image-ubuntu-24-amd64'] }}"
   ```

## Composite Actions

Located in `.github/actions/`, these reusable components include:

- **`cargo-workspace`**: Execute cargo commands on multiple workspace packages
- **`package-deb`**: Create Debian (.deb) packages using cargo-deb
- **`package-rpm`**: Create RPM packages using cargo-generate-rpm
- **`setup-rust`**: Configure Rust environment in containers
- **`setup-macos-build-environment`**: Install macOS dependencies (GTK, protobuf)
- **`upload-artifacts`**: Upload build artifacts with standardized naming

## Configuration

JSON files in `.github/config/` drive the build matrix:

### `global.json`
Central configuration for cross-platform settings:
```json
{
  "registry": "ghcr.io",
  "rust": {
    "components": { "format": "rustfmt", "lint": "clippy", "docs": "rust-docs" },
    "cargo_packages": { "audit": "cargo-deny" }
  },
  "tools": { "go_version": "1.22" }
}
```

### `platforms.json`
Runner and OS image mappings:
```json
{
  "runners": { "linux": { "amd64": "ubuntu-latest", "arm64": "ubuntu-2404-l-arm" } },
  "os_images": { "linux": { "default": "ubuntu-24" } },
  "targets": { "windows": { "default": "x86_64-pc-windows-gnu" } }
}
```

### `{platform}-jobs.json`
Defines crate groups and job artifacts:
```json
{
  "crate_groups": [
    { "name": "client", "crates": ["aperturec-client-gtk3"], "image": "client-image" }
  ],
  "jobs": { "build": { "artifacts": { "client": [{"name-prefix": "client", "path": "..."}] } } }
}
```

### `{platform}-images.json`
Docker image build matrix configuration:
```json
{
  "os": [{ "image_upstream": "ubuntu:24.04", "image_name": "ubuntu-24" }],
  "target": ["rust-image", "client-image"],
  "platform_runner": [{ "platform": "linux/amd64", "runner": "ubuntu-latest" }]
}
```

## Dependency Updates

Dependabot is configured via `.github/dependabot.yml` to:
- Update Rust (Cargo) workspace dependencies weekly
- Update GitHub Actions references weekly
- Attempt Docker base image updates for `ci/images/` weekly
All PRs are labeled `dependencies` and use `chore(deps)`-style commit messages.
