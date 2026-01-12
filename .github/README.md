# ApertureC CI/CD Pipeline

This directory contains GitHub Actions workflows and composite actions for ApertureC's continuous integration and release automation.

## Workflows

### `check-test.yml` - Continuous Integration
Runs on pull requests and pushes to main:
- Builds a Docker image with all dependencies (Ubuntu 24.04)
- Runs code formatting checks (`cargo fmt`)
- Runs license and security audits (`cargo deny`)
- Runs workspace-wide checks and clippy
- Runs all tests

### `build-package.yml` - Build & Package
Creates release artifacts for all platforms. Runs on:
- Pull requests from `release-please--branches--main` (release PRs)
- Manual trigger via `workflow_dispatch`

Builds and packages:
- **Linux**: Client and server for Ubuntu 22/24, Fedora 42/43, CentOS Stream 9 (amd64 and arm64)
- **Windows**: Client for amd64 (cross-compiled from Linux)
- **macOS**: Client for arm64

Package formats: .deb, .rpm, .msi, .tar.gz, .zip

### `automated-release.yml` - Automated Release
Runs on pushes to main:
- Uses release-please to manage releases and changelogs
- Downloads artifacts from Build & Package workflow
- Attaches artifacts to GitHub releases
- Pushes packages to packagecloud
- Deploys Windows MSI to GitHub Pages
- Triggers macOS app build in separate repository

### `scheduled-image-builds.yml` - Weekly Image Refresh
Runs weekly to rebuild all Docker images with fresh base images and dependencies.

## Composite Actions

### `build-docker-image/`
Builds and pushes a single Docker image with digest output.

**Inputs:**
- `target`: Docker build target (e.g., client-image, server-image)
- `upstream_image`: Base image
- `os_variant`: debian or fedora
- `os_name`: For tagging (e.g., ubuntu-22)
- `platform`: e.g., linux/amd64
- `use_registry_cache`: Whether to use registry cache as a cache source (true/false)
- `registry_password`: Authentication token

**Outputs:**
- `digest`: SHA256 digest of built image
- `image_ref`: Full reference with digest (e.g., `ghcr.io/org/repo/image@sha256:...`)

### `setup-rust/`
Configures Rust environment with caching.

**Inputs:**
- `toolchain`: Rust toolchain version (default: stable)
- `components`: Rust components to install (e.g., rustfmt,clippy)
- `cargo-packages`: Cargo packages to install (e.g., cargo-deny)
- `cache-prefix`: Cache key prefix for differentiation

## Docker Images

Images are built from `ci/images/Dockerfile` with multi-stage builds:

- **Base images**: Ubuntu/Fedora with build tools
- **Rust images**: Base + Rust toolchain + cargo-deb/cargo-generate-rpm
- **FIPS-capable images**: Rust + Go compiler
- **Client images**: FIPS + GTK3 dependencies
- **Server images**: FIPS + X11/display dependencies
- **Windows cross-compilation**: Fedora + MinGW + MSI tools

Images are tagged as: `{target}-{os}-{arch}` (e.g., `client-image-ubuntu-24-arm64`)

## Release Process

1. Commits to main trigger release-please
2. Release-please creates/updates a PR with version bumps
3. Build & Package workflow runs on the release PR
4. When release PR is merged, automated-release workflow:
   - Creates git tags and GitHub releases
   - Downloads build artifacts
   - Attaches artifacts to releases
   - Publishes to package repositories
