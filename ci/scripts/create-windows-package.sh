#!/bin/bash

set -eux

mkdir -p package
cp aperturec-client.exe package

for DLL in $(peldd package/*.exe -t --ignore-errors)
    do cp "$DLL" package
done

mkdir -p package/share/{themes,gtk-3.0,glib-2.0}
cp -r "$GTK_INSTALL_PATH/share/glib-2.0/schemas" package/share/glib-2.0/
cp -r "$GTK_INSTALL_PATH/share/icons" package/share/icons

find package -maxdepth 1 -type f -exec mingw-strip {} +

(cd package && zip -qr ../aperturec-client-win.zip .)
rm -rf package
