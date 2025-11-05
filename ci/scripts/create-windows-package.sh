#!/bin/bash

set -euxo pipefail

# derive product version from root Cargo.toml
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
ROOT_DIR=$(dirname "$(dirname "$SCRIPT_DIR")")
SRC_DIR="${ROOT_DIR}/aperturec-client-gtk4"
version=$(
  grep -m1 '^version *= *"' "$SRC_DIR/Cargo.toml" \
    | sed -E 's/.*version *= *"([^"]+)".*/\1/'
)

mkdir -p package
cp aperturec-client.exe package

for DLL in $(peldd package/*.exe -t --ignore-errors)
    do cp "$DLL" package
done

mkdir -p package/share/{themes,gtk-4.0,glib-2.0}
cp -r "$GTK_INSTALL_PATH/share/glib-2.0/schemas" package/share/glib-2.0/
cp -r "$GTK_INSTALL_PATH/share/icons" package/share/icons

find package -maxdepth 1 -type f -exec mingw-strip {} +

pandoc -s -f markdown -t rtf -o package/License.rtf LICENSE

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
    --ext ui \
    -D SourceDir="$(pwd)/" \
    -D Win64=yes \
    -D Version="${version}" \
    -o "../aperturec-client_${version}".msi \
    "${ROOT_DIR}/ci/package.wxs" files.wxs

  # pull jsign
  JSIGN_URI="https://github.com/ebourg/jsign/releases/download/7.1/jsign-7.1.jar"
  curl -O ${JSIGN_URI} -L

  # The following environment variables are GitHub secrets:
  #   AZURE_APP_ID
  #   AZURE_APP_SECRET
  #   AZURE_TENNANT_ID
  #   AZURE_CERT_ALIAS

  # sign into azure and fetch temporary access token
  az login \
    --service-principal \
    --username "${AZURE_APP_ID}" \
    --password "${AZURE_APP_SECRET}" \
    --tenant "${AZURE_TENANT_ID}"

  AZ_TOKEN=$(
    az account get-access-token --resource https://codesigning.azure.net |
    jq -r .accessToken \
  )

  # sign MSI
  java -jar "${JSIGN_URI##*/}" \
    --storetype TRUSTEDSIGNING \
    --keystore eus.codesigning.azure.net \
    --storepass "${AZ_TOKEN}" \
    --alias "${AZURE_CERT_ALIAS}" \
    --name "ApertureC Client" \
    --url "www.zetier.com" \
    "../aperturec-client_${version}".msi
)

rm -rf package
