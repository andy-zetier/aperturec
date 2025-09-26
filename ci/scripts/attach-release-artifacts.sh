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

get_os_label() {
    local dir_name="$1"
    local os_arch="${dir_name#*-}"
    os_arch="${os_arch%-pkg}"

    if [[ "$os_arch" =~ ubuntu-([0-9]+) ]]; then
        echo "ubuntu${BASH_REMATCH[1]}"
    elif [[ "$os_arch" =~ fedora-([0-9]+) ]]; then
        echo "fc${BASH_REMATCH[1]}"
    elif [[ "$os_arch" =~ centos-stream([0-9]+) ]]; then
        echo "el${BASH_REMATCH[1]}"
    fi
}

rename_package() {
    local file="$1"
    local os_label="$2"
    local basename
    basename=$(basename "$file")

    if [[ "$basename" =~ ^(.+)_([0-9].+-[0-9]+)_(.+)\.deb$ ]]; then
        echo "${BASH_REMATCH[1]}_${BASH_REMATCH[2]}+${os_label}_${BASH_REMATCH[3]}.deb"
    elif [[ "$basename" =~ ^(.+-[0-9].+)-([0-9]+)\.(.+)\.rpm$ ]]; then
        echo "${BASH_REMATCH[1]}-${BASH_REMATCH[2]}.${os_label}.${BASH_REMATCH[3]}.rpm"
    else
        echo "$basename"
    fi
}

upload_linux_packages() {
    local prefix="$1"
    local temp_dir
    temp_dir=$(mktemp -d)
    trap "rm -rf '$temp_dir'" EXIT

    for pkg_dir in "$ARTIFACTS_DIR"/"${prefix}"-*-pkg/; do
        [[ -d "$pkg_dir" ]] || continue

        local dir_basename
        dir_basename=$(basename "$pkg_dir")
        local os_label
        os_label=$(get_os_label "$dir_basename")

        if [[ -z "$os_label" ]]; then
            echo "Warning: Unrecognized OS for directory '$dir_basename', skipping." >&2
            continue
        fi

        for pkg in "$pkg_dir"/{debian/*.deb,rpm/*.rpm}; do
            [[ -f "$pkg" ]] || continue
            local new_name
            new_name=$(rename_package "$pkg" "$os_label")
            local temp_file="$temp_dir/$new_name"
            cp "$pkg" "$temp_file"
            gh release upload "$RELEASE_TAG" "$temp_file" --clobber || true
        done
    done
}

case "$PACKAGE_NAME" in
    aperturec-server)
        upload_linux_packages "server"
        ;;

    aperturec-client-gtk3)
        upload_linux_packages "client"

        for pkg in "$ARTIFACTS_DIR"/client-package-macos/*.{tar.gz,zip} "$ARTIFACTS_DIR"/client-package-windows-amd64/*.msi; do
            [[ -f "$pkg" ]] || continue
            # The windows client is packaged in an MSI and we do not need to
            # attach the zip to the release
            [[ "$(basename "$pkg")" != "aperturec-client-win.zip" ]] || continue
            gh release upload "$RELEASE_TAG" "$pkg" --clobber || true
        done
        ;;

    *)
        echo "Error: Unknown package name: $PACKAGE_NAME"
        echo "Supported packages: aperturec-server, aperturec-client-gtk3"
        exit 1
        ;;
esac

