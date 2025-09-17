#!/bin/bash

set -euxo pipefail

PKG_DIR="package/aperturec-client"
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
ROOT_DIR=$(dirname "$(dirname "$SCRIPT_DIR")")
SRC_DIR="${ROOT_DIR}/aperturec-client-gtk3"
version=$(
  grep -m1 '^version *= *"' "$SRC_DIR/Cargo.toml" \
    | sed -E 's/.*version *= *"([^"]+)".*/\1/'
)

mkdir -p "${PKG_DIR}"
cp "${ROOT_DIR}"/target/release/aperturec-client "${PKG_DIR}"
install_name_tool -add_rpath "@executable_path" "${PKG_DIR}/aperturec-client"

# Recursively copy all Homebrew dylib dependencies and their package contents
pushd "${PKG_DIR}" > /dev/null

# queue stores paths relative to PKG_DIR
queue=( "aperturec-client" )
processed_libs=()

while [ ${#queue[@]} -gt 0 ]; do
	lib_rel_path="${queue[0]%:}"
	queue=( "${queue[@]:1}" )
	if printf '%s\n' "${processed_libs[@]}" | grep -qx "$lib_rel_path"; then
		continue
	fi
	processed_libs+=( "$lib_rel_path" )

	if [ ! -f "$lib_rel_path" ]; then
		echo "error: cannot find '$lib_rel_path' to process dependencies" >&2
		exit 1
	fi

	# Add rpath so dylibs will look in this folder.  Skip the main exe.
	if [[ "$lib_rel_path" != "aperturec-client" ]]; then
		# only if @loader_path isn’t already baked in
		if ! otool -l "$lib_rel_path" \
		       | awk '/LC_RPATH/ { getline; print }' \
		       | grep -q "@loader_path"; then
			install_name_tool -add_rpath "@loader_path" "$lib_rel_path"
		fi
	fi

	# Find all Homebrew and @rpath dependencies.
	deps=$(otool -L "$lib_rel_path" \
	       | awk '{print $1}' | tr -d ':' \
	       | grep -E '/usr/local|/opt/homebrew|@rpath' \
	       || true)
	for d in $deps; do
		name=$(basename "$d")
		dep_path_in_brew="$d"

		# Prefer any dylib Cargo built under target/release
		if built=$(find "$ROOT_DIR/target/release" -type f -name "$name" -print -quit 2>/dev/null); then
			if [[ -n "$built" ]]; then
				dep_path_in_brew="$built"
			fi
		fi

		# Otherwise, if this was an @rpath load, look in Brew lib dirs
		if [[ "$d" == @rpath/* && "$dep_path_in_brew" == "$d" ]]; then
			found=""
			for path in "/opt/homebrew/lib" "/usr/local/lib"; do
				if [ -f "$path/$name" ]; then
					dep_path_in_brew="$path/$name"
					found="true"
					break
				fi
			done
			if [ -z "$found" ]; then
				echo "warning: cannot find @rpath dependency '$name' for '$lib_rel_path'" >&2
				continue
			fi
		fi

		real_dep_path=$(realpath "$dep_path_in_brew")

		# If it’s not a Homebrew path, try target/release one more time
		if [[ "$real_dep_path" != */Cellar/* ]]; then
			candidate=$(find "$ROOT_DIR/target/release" -type f -name "$name" -print -quit 2>/dev/null || true)
			if [[ -n "$candidate" ]]; then
				real_dep_path="$candidate"
			else
				echo "warning: dependency '$d' (real path $real_dep_path) not in Homebrew or build outputs, skipping" >&2
				continue
			fi
		fi

		# dest_name is the flat filename we’ll ship
		dest_name=$(basename "$real_dep_path")

		# copy that one dylib into PKG_DIR root if we haven’t already
		if [ ! -f "$dest_name" ]; then
			echo "Copying $dest_name from $real_dep_path"
			cp "$real_dep_path" "./$dest_name"
		fi

		# rewrite the linkage inside the current binary/lib
		install_name_tool -change "$d" "@rpath/$dest_name" "$lib_rel_path"

		# queue up the new dylib so we rewrite its deps in turn
		if file "$dest_name" | grep -q 'Mach-O'; then
			if ! printf '%s\n' "${processed_libs[@]}" | grep -qx "$dest_name" \
			  && ! printf '%s\n' "${queue[@]}" | grep -qx "$dest_name"; then
				queue+=( "$dest_name" )
			fi
		fi
	done
done
popd > /dev/null

cd "${PKG_DIR}"/..
zip -r "${ROOT_DIR}"/aperturec-client-macos-with-all-dependencies_"${version}".zip ./*
