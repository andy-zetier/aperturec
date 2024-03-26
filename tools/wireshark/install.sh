#!/bin/bash

set -eu

LUA_SCRIPT="aperturec.lua"
SCRIPT_DIR=$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )

DEFAULT_INSTALL_DIR="$HOME/.local/lib/wireshark/plugins"
read -r "Wireshark plugin directory [$DEFAULT_INSTALL_DIR]: " INSTALL_DIR
INSTALL_DIR=${INSTALL_DIR:-$DEFAULT_INSTALL_DIR}
if [ ! -d "$INSTALL_DIR" ]; then
  mkdir -p "$INSTALL_DIR"
fi

cp "$SCRIPT_DIR/$LUA_SCRIPT" "$INSTALL_DIR"

echo "Installation script complete! Finish configuration in the Wireshark GUI according to the README.md"
