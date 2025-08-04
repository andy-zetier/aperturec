#!/bin/bash

set -euxo pipefail

# ==== derive product version from root Cargo.toml ====
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
ROOT_DIR=$(dirname "$(dirname "$SCRIPT_DIR")")
version=$(
  grep -m1 '^version *= *"' "$ROOT_DIR/Cargo.toml" \
    | sed -E 's/.*version *= *"([^"]+)".*/\1/'
)


mkdir -p package
cp aperturec-client.exe package

for DLL in $(peldd package/*.exe -t --ignore-errors)
    do cp "$DLL" package
done

mkdir -p package/share/{themes,gtk-3.0,glib-2.0}
cp -r "$GTK_INSTALL_PATH/share/glib-2.0/schemas" package/share/glib-2.0/
cp -r "$GTK_INSTALL_PATH/share/icons" package/share/icons

find package -maxdepth 1 -type f -exec mingw-strip {} +

(
  cd package

  # zip up exe + DLLs
  zip -qr ../aperturec-client-win.zip .

  # generate file/directory fragment
  find "$(pwd)/" -type f | wixl-heat \
    -p "$(pwd)/" \
    --var var.SourceDir \
    --component-group ClientFiles \
    --directory-ref INSTALLFOLDER \
    --win64 > files.wxs

  # compile MSI
  wixl -v -a x64 \
	  -D SourceDir="$(pwd)/" \
	  -D Win64=yes \
	  -D Version="${version}" \
	  -o "../aperturec-client_${version}".msi \
	  "${ROOT_DIR}/ci/package.wxs" files.wxs
)

rm -rf package
