#
# Windows native build script (builds the GTK client executable).
#
# CI environment / local use:
# - GitHub-hosted Windows runner label `windows-2025`
#   https://github.com/actions/runner-images/blob/main/images/windows/Windows2025-Readme.md
# - Can also be run on local Windows hosts with PowerShell
#
# Dependencies:
# - CMake
# - Cargo / Rust toolchain
# - Git
# - LLVM
# - Python 3.14 (launcher `py`)
# - curl.exe (optional)
# - tar (optional)
# - Visual Studio 2022 with C++ workloads
# - vcpkg
#
# Environment variables
# ---------------------
# Required:
# - None
#
# Optional overrides:
# - VCPKG_ROOT: vcpkg install root. If unset, auto-resolved via standard locations/VS install.
# - VCPKG_INSTALLED_DIR: prebuilt vcpkg installed directory. If set and contains
#   x64-windows and x64-windows-static, vcpkg install is skipped.
# - GTK_ROOT: GTK install root. If set and contains gtk-4-1.dll and adwaita-1-0.dll,
#   gvsbuild is skipped. Otherwise GTK is built via gvsbuild and auto-resolved.
# - GVSBUILD_RELEASE_TAG: GitHub release tag for prebuilt GTK download (default: latest).
# - GVSBUILD_ASSET_NAME: GitHub release asset name for GTK zip (default: first GTK4_*_x64.zip).
# - GVSBUILD_ZIP_URL: Direct URL to GTK zip (overrides tag/asset lookup).
# - PYTHON_EXE: Python launcher (defaults to "py").
# - PYTHON_ARGS: Python args (defaults to "-3.14").
#
# Set by this script:
# - VCPKG_MANIFEST_DIR, VCPKG_INSTALLED_DIR, VCPKGRS_DYNAMIC, OPENSSL_DIR, PROTOC, CL,
#   PKG_CONFIG_PATH, PATH, LIB.

#
# Environment setup: read optional env overrides and prepare a temp vcpkg manifest.
#
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$VcpkgRoot = $env:VCPKG_ROOT
$GtkRoot = $env:GTK_ROOT
$PythonExe = if ($env:PYTHON_EXE) { $env:PYTHON_EXE } else { "py" }
$PythonArgs = @("-3.14")
if ($env:PYTHON_ARGS) {
  $PythonArgs = $env:PYTHON_ARGS -split '\s+' | Where-Object { $_ -ne "" }
}

$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Resolve-Path (Join-Path $scriptRoot "..\..")
$tempManifestRoot = Join-Path $env:TEMP ("aperturec-vcpkg-" + [Guid]::NewGuid().ToString("N"))
$manifestPath = Join-Path $tempManifestRoot "vcpkg.json"

New-Item -ItemType Directory -Path $tempManifestRoot | Out-Null
$manifest = @"
{
  "name": "aperturec",
  "version-string": "0.0.0",
  "dependencies": [
    "openssl",
    "protobuf"
  ]
}
"@
$manifest | Set-Content -Path $manifestPath -NoNewline
$env:VCPKG_MANIFEST_DIR = $tempManifestRoot
Write-Host "Set VCPKG_MANIFEST_DIR=$env:VCPKG_MANIFEST_DIR"

#
# Helper functions for command execution and path resolution.
#
function Require-Command {
  param(
    [Parameter(Mandatory = $true)][string]$Name,
    [Parameter(Mandatory = $true)][string]$Hint
  )
  if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
    throw "$Name not found. $Hint"
  }
}

function Invoke-Checked {
  param(
    [Parameter(Mandatory = $true)][string]$Label,
    [Parameter(Mandatory = $true)][string]$File,
    [Parameter()][string[]]$Args = @()
  )
  & $File @Args
  if ($LASTEXITCODE -ne 0) {
    throw "$Label failed with exit code $LASTEXITCODE"
  }
}

function Resolve-PipxBinDir {
  param(
    [Parameter(Mandatory = $true)][string]$PythonExe,
    [Parameter()][string[]]$PythonArgs = @()
  )
  try {
    $pipxBin = & $PythonExe @PythonArgs -m pipx environment --value PIPX_BIN_DIR 2>$null
    if ($LASTEXITCODE -ne 0) {
      return $null
    }
    $pipxBin = $pipxBin | Select-Object -First 1
    if ($pipxBin -and (Test-Path $pipxBin)) {
      return $pipxBin
    }
  } catch {
    return $null
  }
  return $null
}

function Resolve-GtkRoot {
  param([string]$PreferredRoot)
  if ($PreferredRoot) {
    if (Test-GtkRoot -Root $PreferredRoot) {
      return $PreferredRoot
    }
    return $null
  }
  $candidates = @(
    $env:GVSBUILD_INSTALL_PATH,
    $env:GVSBUILD_PREFIX,
    "C:\gtk",
    "C:\gtk-build\gtk\x64\release"
  ) | Where-Object { $_ -and (Test-GtkRoot -Root $_) }
  $candidates = @($candidates)
  if ($candidates.Count -gt 0) {
    return $candidates[0]
  }
  return $null
}

function Resolve-VcpkgRoot {
  if ($env:VCPKG_ROOT) {
    return $env:VCPKG_ROOT
  }

  $candidates = @(
    "C:\vcpkg",
    (Join-Path $env:USERPROFILE "vcpkg"),
    (Join-Path $env:LOCALAPPDATA "vcpkg")
  ) | Where-Object { $_ -and (Test-Path $_) }

  foreach ($candidate in $candidates) {
    $exe = Join-Path $candidate "vcpkg.exe"
    if (Test-Path $exe) {
      return $candidate
    }
  }

  $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
  if (Test-Path $vswhere) {
    $installPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($installPath) {
      $vsVcpkg = Join-Path $installPath "VC\vcpkg"
      if (Test-Path $vsVcpkg) {
        return $vsVcpkg
      }
    }
  }

  return "C:\vcpkg"
}

function Test-GtkRoot {
  param([string]$Root)
  if (-not $Root) {
    return $false
  }
  $gtk4Dll = Join-Path $Root "bin\gtk-4-1.dll"
  return (Test-Path $gtk4Dll)
}

function Get-GvsbuildRelease {
  param([string]$ReleaseTag)
  $apiUrl = if ($ReleaseTag) {
    "https://api.github.com/repos/wingtk/gvsbuild/releases/tags/$ReleaseTag"
  } else {
    "https://api.github.com/repos/wingtk/gvsbuild/releases/latest"
  }
  return Invoke-RestMethod -Uri $apiUrl -Headers @{ "User-Agent" = "aperturec-ci" }
}

function Resolve-GvsbuildGtkAsset {
  param(
    [string]$ReleaseTag,
    [string]$AssetName,
    [string]$ZipUrl
  )
  if ($ZipUrl) {
    return @{
      Url = $ZipUrl
      Name = (Split-Path $ZipUrl -Leaf)
      Tag = $ReleaseTag
      RequestedAsset = $AssetName
    }
  }
  $release = Get-GvsbuildRelease -ReleaseTag $ReleaseTag
  $assets = @($release.assets)
  if ($AssetName) {
    $asset = $assets | Where-Object { $_.name -eq $AssetName } | Select-Object -First 1
  } else {
    $asset = $assets | Where-Object { $_.name -match '^GTK4_.*_x64\.zip$' } | Select-Object -First 1
  }
  if (-not $asset) {
    return $null
  }
  return @{
    Url = $asset.browser_download_url
    Name = $asset.name
    Tag = $release.tag_name
    RequestedAsset = $AssetName
  }
}

function Download-GvsbuildGtk {
  param(
    [Parameter(Mandatory = $true)][string]$TargetRoot,
    [string]$ReleaseTag,
    [string]$AssetName,
    [string]$ZipUrl
  )
  $asset = Resolve-GvsbuildGtkAsset -ReleaseTag $ReleaseTag -AssetName $AssetName -ZipUrl $ZipUrl
  if (-not $asset) {
    Write-Warning "No GTK asset found from gvsbuild releases (tag=$ReleaseTag, asset=$AssetName)."
    return $null
  }
  $tagLabel = if ($asset.Tag) { $asset.Tag } else { "<unknown>" }
  $requested = if ($asset.RequestedAsset) { $asset.RequestedAsset } else { "<auto>" }
  Write-Host "Selected GTK release tag: $tagLabel"
  Write-Host "Selected GTK asset: $($asset.Name) (requested: $requested)"

  if (-not (Test-Path $TargetRoot)) {
    New-Item -ItemType Directory -Path $TargetRoot | Out-Null
  }

  $zipPath = Join-Path $env:TEMP $asset.Name
  Write-Host "Downloading GTK from $($asset.Url)"
  if (Get-Command curl.exe -ErrorAction SilentlyContinue) {
    Write-Host "Using curl.exe for download."
    & curl.exe -L --retry 3 --retry-delay 2 -o $zipPath $asset.Url
  } else {
    Write-Host "Using Invoke-WebRequest for download."
    Invoke-WebRequest -Uri $asset.Url -OutFile $zipPath -Headers @{ "User-Agent" = "aperturec-ci" }
  }
  Write-Host "Extracting GTK archive to $TargetRoot"
  if (Get-Command tar -ErrorAction SilentlyContinue) {
    Write-Host "Using tar for extraction."
    & tar -xf $zipPath -C $TargetRoot
  } else {
    Write-Host "Using Expand-Archive for extraction."
    Expand-Archive -Path $zipPath -DestinationPath $TargetRoot -Force
  }
  Remove-Item -Force $zipPath

  if (Test-GtkRoot -Root $TargetRoot) {
    Write-Host "GTK present at $TargetRoot after extraction."
    return $TargetRoot
  }

  Write-Warning "GTK not found in extracted archive at $TargetRoot."
  return $null
}

if (-not $VcpkgRoot) {
  $VcpkgRoot = Resolve-VcpkgRoot
}

#
# Verify required tools are installed and resolve key paths.
#
Write-Host "Resolved VCPKG_ROOT=$VcpkgRoot"

if ($env:GTK_ROOT -and -not (Test-Path $env:GTK_ROOT)) {
  throw "GTK root not found at $env:GTK_ROOT (from GTK_ROOT). Update/unset GTK_ROOT or create that directory."
}

if (-not (Test-Path $VcpkgRoot)) {
  if ($env:VCPKG_ROOT) {
    throw "vcpkg not found at $VcpkgRoot (from VCPKG_ROOT). Install vcpkg there or update/unset VCPKG_ROOT."
  }
  throw "vcpkg not found at $VcpkgRoot (auto-resolved). Set VCPKG_ROOT or install vcpkg to that path."
}

$vcpkgExe = Join-Path $VcpkgRoot "vcpkg.exe"
if (-not (Test-Path $vcpkgExe)) {
  throw "vcpkg.exe not found at $vcpkgExe. Ensure vcpkg is installed."
}

try {

Require-Command -Name "git" -Hint "Install Git for Windows and ensure it is on PATH."
Require-Command -Name "cmake" -Hint "Install CMake and ensure it is on PATH."
Require-Command -Name "cargo" -Hint "Install Rust and ensure cargo is on PATH."
Require-Command -Name $PythonExe -Hint "Install the Python launcher ('py') and ensure it is on PATH."

$existingGtk = Resolve-GtkRoot -PreferredRoot $GtkRoot
$gtk4Dll = if ($existingGtk) { Join-Path $existingGtk "bin\gtk-4-1.dll" } else { $null }
$adwaitaDll = if ($existingGtk) { Join-Path $existingGtk "bin\adwaita-1-0.dll" } else { $null }
$needsGtkBuild = -not $existingGtk -or -not (Test-Path $gtk4Dll) -or -not (Test-Path $adwaitaDll)
if (-not $needsGtkBuild) {
  Write-Host "GTK already present at $existingGtk; skipping gvsbuild. To force a rebuild, delete that directory (or C:\\gtk-build\\gtk\\x64\\release) and unset GTK_ROOT/GVSBUILD_*, then rerun."
} else {
  $downloadRoot = if ($GtkRoot) { $GtkRoot } else { "C:\gtk" }
  Write-Host "GTK not found locally; attempting to download prebuilt GTK release into $downloadRoot."
  $downloadedGtk = $null
  try {
    $downloadedGtk = Download-GvsbuildGtk `
      -TargetRoot $downloadRoot `
      -ReleaseTag $env:GVSBUILD_RELEASE_TAG `
      -AssetName $env:GVSBUILD_ASSET_NAME `
      -ZipUrl $env:GVSBUILD_ZIP_URL
  } catch {
    Write-Warning "Failed to download prebuilt GTK: $($_.Exception.Message)"
  }

  if ($downloadedGtk) {
    Write-Host "Downloaded GTK to $downloadedGtk"
    $existingGtk = $downloadedGtk
    $GtkRoot = $downloadedGtk
    $needsGtkBuild = $false
  }

  if ($needsGtkBuild) {
  #
  # Setup and build GTK4 via gvsbuild.
  #
  Write-Host "==> Setup and build GTK4 via gvsbuild"

  Invoke-Checked -Label "pipx install" -File $PythonExe -Args (@($PythonArgs) + @("-m", "pip", "install", "--user", "pipx"))
  Invoke-Checked -Label "gvsbuild install" -File $PythonExe -Args (@($PythonArgs) + @("-m", "pipx", "install", "gvsbuild"))

  $pipxBin = Resolve-PipxBinDir -PythonExe $PythonExe -PythonArgs $PythonArgs
  if ($pipxBin -and ($env:Path -notlike "*$pipxBin*")) {
    $env:Path = "$pipxBin;$env:Path"
    Write-Host "Set PATH=$env:Path"
  }

  # Prevent MSVC compiler from running out of heap space
  $env:CL = "/Zm200"
  Write-Host "Set CL=$env:CL"

  if (Get-Command gvsbuild -ErrorAction SilentlyContinue) {
    Invoke-Checked -Label "gvsbuild build" -File "gvsbuild" -Args @("build", "gtk4", "libadwaita")
  } else {
    Write-Warning "gvsbuild command not found on PATH; falling back to Python module install"
    Invoke-Checked -Label "gvsbuild pip install" -File $PythonExe -Args (@($PythonArgs) + @("-m", "pip", "install", "--user", "gvsbuild"))
    Invoke-Checked -Label "gvsbuild build (module)" -File $PythonExe -Args (@($PythonArgs) + @("-m", "gvsbuild", "build", "gtk4", "libadwaita"))
  }
  }
}

$GtkRoot = Resolve-GtkRoot -PreferredRoot $GtkRoot
if (-not $GtkRoot) {
  if ($env:GTK_ROOT) {
    throw "GTK root not found at $env:GTK_ROOT (from GTK_ROOT). Update/unset GTK_ROOT or ensure gvsbuild installed to a standard location."
  }
  throw "GTK root not found (auto-resolved). Set GTK_ROOT or ensure gvsbuild installed to a standard location."
}

Write-Host "Resolved GTK_ROOT=$GtkRoot"

$env:PKG_CONFIG_PATH = (Join-Path $GtkRoot "lib\pkgconfig")
$env:Path = (Join-Path $GtkRoot "bin") + ";$env:Path"
$env:Lib = (Join-Path $GtkRoot "lib") + ";$env:Lib"
Write-Host "Set PKG_CONFIG_PATH=$env:PKG_CONFIG_PATH"
Write-Host "Set PATH=$env:Path"
Write-Host "Set LIB=$env:Lib"

#
# Install vcpkg dependencies in manifest mode and set env hints for build scripts.
#
Write-Host "==> Install vcpkg dependencies"
$manifestNeedsBaseline = $false
if ((Get-Content -Raw -Path $manifestPath) -notmatch '"builtin-baseline"\s*:') {
  $manifestNeedsBaseline = $true
}
$vcpkgInstalledDir = if ($env:VCPKG_INSTALLED_DIR) { $env:VCPKG_INSTALLED_DIR } else { Join-Path $repoRoot "vcpkg_installed" }
$env:VCPKG_INSTALLED_DIR = $vcpkgInstalledDir
Write-Host "Set VCPKG_INSTALLED_DIR=$env:VCPKG_INSTALLED_DIR"
if ($env:VCPKG_INSTALLED_DIR -and (Test-Path $vcpkgInstalledDir)) {
  $tripletDirs = @(
    (Join-Path $vcpkgInstalledDir "x64-windows"),
    (Join-Path $vcpkgInstalledDir "x64-windows-static")
  )
  $missingTriplets = @()
  foreach ($dir in $tripletDirs) {
    if (-not (Test-Path $dir)) {
      $missingTriplets += $dir
    }
  }
  if ($missingTriplets.Count -gt 0) {
    throw "VCPKG_INSTALLED_DIR is set to $vcpkgInstalledDir but missing triplets: $($missingTriplets -join ', ')."
  }
  Write-Host "Using existing VCPKG_INSTALLED_DIR; skipping vcpkg install."
} else {
  Push-Location $tempManifestRoot
  try {
    if ($manifestNeedsBaseline) {
      Invoke-Checked -Label "vcpkg baseline" -File $vcpkgExe -Args @("x-update-baseline", "--add-initial-baseline")
    }
    Invoke-Checked -Label "vcpkg install (x64-windows)" -File $vcpkgExe -Args @(
      "install",
      "--x-install-root",
      $vcpkgInstalledDir,
      "--triplet",
      "x64-windows"
    )
    Invoke-Checked -Label "vcpkg install (x64-windows-static)" -File $vcpkgExe -Args @(
      "install",
      "--x-install-root",
      $vcpkgInstalledDir,
      "--triplet",
      "x64-windows-static"
    )
  } finally {
    Pop-Location
  }
}

$env:VCPKGRS_DYNAMIC = 1
$env:OPENSSL_DIR = (Join-Path $vcpkgInstalledDir "x64-windows-static")
Write-Host "Set VCPKGRS_DYNAMIC=$env:VCPKGRS_DYNAMIC"
Write-Host "Set OPENSSL_DIR=$env:OPENSSL_DIR"

#
# Resolve protoc (protobuf compiler) for code generation.
#
Write-Host "==> Resolve protoc"
$env:PROTOC = (Join-Path $vcpkgInstalledDir "x64-windows\tools\protobuf\protoc.exe")
Write-Host "Set PROTOC=$env:PROTOC"
if (-not (Test-Path $env:PROTOC)) {
  throw "protoc not found at $env:PROTOC. Ensure vcpkg installed protobuf for the x64-windows triplet."
}

#
# Build the Windows client in release mode.
#
Write-Host "==> Build ApertureC"
Invoke-Checked -Label "cargo build" -File "cargo" -Args @("build", "--release", "-p", "aperturec-client-gtk4", "--target", "x86_64-pc-windows-msvc")

} finally {
  if (Test-Path $tempManifestRoot) {
    try {
      Remove-Item -Recurse -Force $tempManifestRoot
    } catch {
      Write-Warning "Failed to delete temp vcpkg manifest directory: $tempManifestRoot"
    }
  }
}
