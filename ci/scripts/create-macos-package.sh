#! /bin/bash

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

for dylib in \
	$(otool -L "${PKG_DIR}"/aperturec-client \
		| grep "@rpath" \
		| awk '{print $1}')
do
	dylib="${dylib##*/}"
	find "${ROOT_DIR}"/target/release \
		-type f \
		-name "${dylib}" \
		-exec cp {} ./"${PKG_DIR}"/ \; \
		-quit;
	install_name_tool \
		-change \
		@rpath/"${dylib}" \
		@executable_path/"${dylib}" "${PKG_DIR}"/aperturec-client
done

cd "${PKG_DIR}"/..
tar -czf "${ROOT_DIR}"/aperturec-client-macos_"${version}".tar.gz ./*
