#!/bin/bash
set -euox pipefail

PACKAGE_NAME="${1:-}"
RELEASE_TAG="${2:-}"
ARTIFACTS_DIR="${3:-./artifacts}"

if [[ -z "$PACKAGE_NAME" || -z "$RELEASE_TAG" ]]; then
    echo "Usage: $0 <package_name> <release_tag> [artifacts_dir]"
    echo "  package_name: aperturec-server or aperturec-client-gtk3"
    echo "  release_tag: The git tag for the release"
    echo "  artifacts_dir: Directory containing downloaded artifacts (default: ./artifacts)"
    exit 1
fi

if [[ ! -d "$ARTIFACTS_DIR" ]]; then
    echo "Error: Artifacts directory not found: $ARTIFACTS_DIR"
    exit 1
fi

# Check for package_cloud command (skip in dry-run mode for testing)
if [[ "${PACKAGECLOUD_DRY_RUN:-}" != "1" ]] && ! command -v package_cloud &> /dev/null; then
    echo "Error: package_cloud command not found. Please install with: gem install package_cloud"
    exit 1
fi

# Check for PACKAGECLOUD_TOKEN
if [[ -z "${PACKAGECLOUD_TOKEN:-}" ]]; then
    echo "Error: PACKAGECLOUD_TOKEN environment variable is not set"
    exit 1
fi

# Function to map OS labels to PackageCloud distribution names
get_packagecloud_distro() {
    local dir_name="$1"

    case "$dir_name" in
        *ubuntu-22*)
            echo "ubuntu/jammy"
            ;;
        *ubuntu-24*)
            echo "ubuntu/noble"
            ;;
        *fedora-42*)
            echo "fedora/42"
            ;;
        *fedora-43*)
            echo "fedora/43"
            ;;
        *centos-stream9*)
            echo "el/9"
            ;;
        *)
            echo ""
            ;;
    esac
}

# Function to determine the target repository based on package name
get_target_repo() {
    local base_package="$1"

    case "$base_package" in
        aperturec-server)
            echo "zetier/aperturec"
            ;;
        aperturec-client-gtk3)
            # Client packages go to aperturec-client repo
            echo "zetier/aperturec-client"
            ;;
        *)
            echo ""
            ;;
    esac
}

# Function to push a package to PackageCloud
push_package() {
    local package_file="$1"
    local distro="$2"
    local repo="$3"

    if [[ -z "$distro" || -z "$repo" ]]; then
        echo "Warning: Skipping $package_file - unable to determine distribution or repository"
        return 1
    fi

    # Use an array for safer command execution
    local push_cmd_args=(package_cloud push "${repo}/${distro}" "${package_file}" --skip-errors)

    if [[ "${PACKAGECLOUD_DRY_RUN:-}" == "1" ]]; then
        echo "[DRY RUN] Would execute: ${push_cmd_args[*]}"
        return 0  # Success in dry-run mode
    else
        echo "Pushing $package_file to $repo/$distro..."
        if "${push_cmd_args[@]}"; then
            echo "✓ Successfully pushed $(basename "$package_file")"
            return 0
        else
            echo "⚠ Failed to push $(basename "$package_file") (may already exist)"
            return 1
        fi
    fi
}

# Main upload function for Linux packages
upload_linux_packages() {
    local prefix="$1"
    local base_package="$2"
    local total_pushed=0
    local total_failed=0

    echo "Processing $base_package packages..."

    for pkg_dir in "$ARTIFACTS_DIR"/"${prefix}"-*-pkg/; do
        [[ -d "$pkg_dir" ]] || continue

        local dir_basename
        dir_basename=$(basename "$pkg_dir")

        # Get the PackageCloud distribution name
        local distro
        distro=$(get_packagecloud_distro "$dir_basename")

        if [[ -z "$distro" ]]; then
            echo "Warning: Unable to determine distribution for $dir_basename"
            continue
        fi

        # Get the target repository
        local repo
        repo=$(get_target_repo "$base_package")

        echo "Processing packages from $dir_basename (→ $distro)..."

        # Process .deb and .rpm packages
        for pkg in "$pkg_dir"debian/*.deb "$pkg_dir"rpm/*.rpm; do
            [[ -f "$pkg" ]] || continue
            if push_package "$pkg" "$distro" "$repo"; then
                total_pushed=$((total_pushed + 1))
            else
                total_failed=$((total_failed + 1))
            fi
        done
    done

    echo ""
    echo "Summary for $base_package:"
    echo "  Packages pushed: $total_pushed"
    if [[ $total_failed -gt 0 ]]; then
        echo "  Packages failed: $total_failed"
    fi
}

# Main execution
echo "========================================="
echo "PackageCloud Upload for $PACKAGE_NAME"
echo "Release Tag: $RELEASE_TAG"
echo "Artifacts Directory: $ARTIFACTS_DIR"
if [[ "${PACKAGECLOUD_DRY_RUN:-}" == "1" ]]; then
    echo "Mode: DRY RUN (no actual uploads)"
fi
echo "========================================="
echo ""

case "$PACKAGE_NAME" in
    aperturec-server)
        upload_linux_packages "server" "aperturec-server"
        ;;

    aperturec-client-gtk3)
        upload_linux_packages "client" "aperturec-client-gtk3"
        ;;

    *)
        echo "Error: Unknown package name: $PACKAGE_NAME"
        echo "Supported packages: aperturec-server, aperturec-client-gtk3"
        exit 1
        ;;
esac

echo ""
echo "PackageCloud upload complete!"
