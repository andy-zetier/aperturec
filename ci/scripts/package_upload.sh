#!/bin/bash
set -e # Exit on error
set -u # Exit on unset variables
set -x # Print each command as it is run

# From https://stackoverflow.com/questions/192292/how-best-to-include-other-scripts
REPO="${BASH_SOURCE%/*}/../.."
if [[ ! -d "${REPO}" ]]; then REPO="$PWD/../"; fi

# Define the base directory
BASE_DIR="$REPO/target"

# Initialize a counter for .deb/.rpm files
counter=0

# Loop through each distribution (e.g., debian, ubuntu, fedora)
for distro in "$BASE_DIR"/*; do
    if [ -d "$distro" ]; then
        distro_name=$(basename "$distro")

        # Loop through each release (e.g., buster, bionic)
        for release in "$distro"/*; do
            if [ -d "$release" ]; then
                release_name=$(basename "$release")

                # Loop through each .deb/.rpm file
                for package in "$release"/*.deb "$release"/*.rpm; do
                    if [ -f "$package" ]; then
                        counter=$((counter + 1))
                        echo "Uploading $package to $distro_name/$release_name ..."

                        # Upload the package
                        package_cloud push zetier/aperturec/"$distro_name"/"$release_name" "$package"
                    fi
                done
            fi
        done
    fi
done

# Check if no .deb/.rpm files were found
if [ $counter -eq 0 ]; then
    echo "Error: No .deb or .rpm files found to upload."
    exit 1
fi
