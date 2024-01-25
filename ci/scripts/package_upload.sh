#!/bin/bash
set -e # Exit on error
set -u # Exit on unset variables
set -x # Print each command as it is run

COMPONENT="$1"

# From https://stackoverflow.com/questions/192292/how-best-to-include-other-scripts
REPO="${BASH_SOURCE%/*}/../.."
if [[ ! -d "${REPO}" ]]; then REPO="$PWD/../"; fi

# Define the base directory
BASE_DIR="$REPO/$COMPONENT/target/"

# Initialize a counter for .deb files
deb_counter=0

# Loop through each distribution (e.g., debian, ubuntu)
for distro in "$BASE_DIR"/*; do
    if [ -d "$distro" ]; then
        distro_name=$(basename "$distro")

        # Loop through each release (e.g., buster, bionic)
        for release in "$distro"/*; do
            if [ -d "$release" ]; then
                release_name=$(basename "$release")

                # Loop through each .deb file
                for deb_package in "$release"/*.deb; do
                    if [ -f "$deb_package" ]; then
                        deb_counter=$((deb_counter + 1))
                        echo "Uploading $deb_package to $distro_name/$release_name ..."

                        # Upload the package
                        package_cloud push zetier/aperturec/$distro_name/$release_name "$deb_package"
                    fi
                done
            fi
        done
    fi
done

# Check if no .deb files were found
if [ $deb_counter -eq 0 ]; then
    echo "Error: No .deb files found to upload."
    exit 1
fi
