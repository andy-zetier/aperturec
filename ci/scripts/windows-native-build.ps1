Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

Write-Host "==> Setup and build GTK4 via gvsbuild"
$env:PKG_CONFIG_PATH = "C:\gtk-build\gtk\x64\release\lib\pkgconfig"
$env:Path = "C:\gtk-build\gtk\x64\release\bin;$env:Path"
$env:Lib = "C:\gtk-build\gtk\x64\release\lib;$env:Lib"

py -3.14 -m pip install --user pipx
py -3.14 -m pipx ensurepath
pipx install gvsbuild

gvsbuild build gtk4

Write-Host "==> Install OpenSSL"
vcpkg install openssl:x64-windows
vcpkg install openssl:x64-windows-static
vcpkg integrate install

$env:VCPKGRS_DYNAMIC = 1
$env:OPENSSL_DIR = 'C:\vcpkg\installed\x64-windows-static'

Write-Host "==> Install protobuf"
winget install -e --id Google.Protobuf --accept-source-agreements
$env:PROTOC = "C:\Users\runneradmin\AppData\Local\Microsoft\WinGet\Links\protoc"

Write-Host "==> Build ApertureC"
cargo build --release --workspace --exclude aperturec-server
cargo build --workspace --exclude aperturec-server
