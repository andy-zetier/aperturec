#
# This script targets the GitHub-hosted Windows runner label `windows-2025`:
#     https://github.com/actions/runner-images/blob/main/images/windows/Windows2025-Readme.md
#
# The following dependencies are required:
#
# - CMake
# - Cargo
# - Git
# - LLVM
# - Python 3.14
# - Rust
# - SSH
# - Visual Studio 2022 with C++ workloads
# - vcpkg
#

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
    if (Test-Path $PreferredRoot) {
      return $PreferredRoot
    }
    return $null
  }
  $candidates = @(
    $env:GVSBUILD_INSTALL_PATH,
    $env:GVSBUILD_PREFIX,
    "C:\gtk-build\gtk\x64\release"
  ) | Where-Object { $_ -and (Test-Path $_) }
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

$existingGtk = Resolve-GtkRoot -PreferredRoot $GtkRoot
$gtk4Dll = if ($existingGtk) { Join-Path $existingGtk "bin\gtk-4-1.dll" } else { $null }
$adwaitaDll = if ($existingGtk) { Join-Path $existingGtk "bin\libadwaita-1.dll" } else { $null }
$needsGtkBuild = -not $existingGtk -or -not (Test-Path $gtk4Dll) -or -not (Test-Path $adwaitaDll)
if (-not $needsGtkBuild) {
  Write-Warning "GTK already present at $existingGtk; skipping gvsbuild. To force a rebuild, delete that directory (or C:\\gtk-build\\gtk\\x64\\release) and unset GTK_ROOT/GVSBUILD_*, then rerun."
} else {
  if (Get-Command gvsbuild -ErrorAction SilentlyContinue) {
    Invoke-Checked -Label "gvsbuild build" -File "gvsbuild" -Args @("build", "gtk4", "libadwaita")
  } else {
    Write-Warning "gvsbuild command not found on PATH; falling back to Python module install"
    Invoke-Checked -Label "gvsbuild pip install" -File $PythonExe -Args (@($PythonArgs) + @("-m", "pip", "install", "--user", "gvsbuild"))
    Invoke-Checked -Label "gvsbuild build (module)" -File $PythonExe -Args (@($PythonArgs) + @("-m", "gvsbuild", "build", "gtk4", "libadwaita"))
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
$vcpkgInstalledDir = Join-Path $repoRoot "vcpkg_installed"
$env:VCPKG_INSTALLED_DIR = $vcpkgInstalledDir
Write-Host "Set VCPKG_INSTALLED_DIR=$env:VCPKG_INSTALLED_DIR"
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

#
# Package the exe, DLL dependencies, and GTK assets into a zip.
#
Write-Host "==> Package ApertureC"
$packageDir = Join-Path $repoRoot "package"
$zipPath = Join-Path $repoRoot "aperturec-client-win.zip"
$targetDir = Join-Path $repoRoot "target\x86_64-pc-windows-msvc\release"
$exePath = Join-Path $targetDir "aperturec-client-gtk4.exe"
$gtkInstallPath = $GtkRoot
$searchPaths = @(
  $targetDir,
  (Join-Path $gtkInstallPath "bin"),
  (Join-Path $vcpkgInstalledDir "x64-windows\bin"),
  (Join-Path $vcpkgInstalledDir "x64-windows-static\bin")
)

if (Test-Path $packageDir) {
  Remove-Item $packageDir -Recurse -Force
}
New-Item -ItemType Directory -Path $packageDir | Out-Null

Copy-Item $exePath -Destination $packageDir

function Resolve-DllPath {
  param(
    [Parameter(Mandatory = $true)][string]$DllName,
    [Parameter(Mandatory = $true)][string[]]$SearchPaths
  )
  foreach ($dir in $SearchPaths) {
    $candidate = Join-Path $dir $DllName
    if (Test-Path $candidate) {
      return $candidate
    }
  }
  return $null
}

function Resolve-DumpbinPath {
  $cmd = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
  if ($null -ne $cmd) {
    return $cmd.Source
  }

  $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
  if (-not (Test-Path $vswhere)) {
    return $null
  }

  $installPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
  if (-not $installPath) {
    return $null
  }

  $msvcRoot = Join-Path $installPath "VC\Tools\MSVC"
  $versions = Get-ChildItem -Path $msvcRoot -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending
  foreach ($version in $versions) {
    $candidate = Join-Path $version.FullName "bin\Hostx64\x64\dumpbin.exe"
    if (Test-Path $candidate) {
      return $candidate
    }
    $candidate = Join-Path $version.FullName "bin\Hostx86\x64\dumpbin.exe"
    if (Test-Path $candidate) {
      return $candidate
    }
  }

  return $null
}

function Get-DependentDllNames {
  param([Parameter(Mandatory = $true)][string]$BinaryPath)
  $dumpbin = Resolve-DumpbinPath
  if (-not $dumpbin) {
    throw "dumpbin.exe not found (ensure VS Build Tools are installed and on PATH)"
  }
  $lines = & $dumpbin /dependents $BinaryPath 2>$null
  if ($LASTEXITCODE -ne 0) {
    throw "dumpbin failed for $BinaryPath"
  }
  $dllMatches = @()
  foreach ($line in $lines) {
    if ($line -match '^\s+([A-Za-z0-9._-]+\.dll)$') {
      $dllMatches += $Matches[1]
    }
  }
  return $dllMatches
}

function Test-SystemDll {
  param([Parameter(Mandatory = $true)][string]$DllName)
  $name = $DllName.ToLowerInvariant()
  if ($name -like "api-ms-win-*" -or $name -like "ext-ms-*") {
    return $true
  }
  $known = @(
    "kernel32.dll",
    "user32.dll",
    "ntdll.dll",
    "combase.dll",
    "ole32.dll",
    "oleaut32.dll",
    "advapi32.dll",
    "ws2_32.dll",
    "shell32.dll",
    "secur32.dll",
    "pdh.dll",
    "psapi.dll",
    "powrprof.dll",
    "crypt32.dll",
    "bcrypt.dll",
    "bcryptprimitives.dll",
    "dwmapi.dll",
    "imm32.dll",
    "setupapi.dll",
    "winmm.dll",
    "hid.dll",
    "opengl32.dll",
    "d3d11.dll",
    "d3d12.dll",
    "dcomp.dll",
    "dxgi.dll",
    "gdi32.dll",
    "comdlg32.dll",
    "msimg32.dll",
    "shlwapi.dll",
    "dnsapi.dll",
    "iphlpapi.dll",
    "usp10.dll",
    "rpcrt4.dll",
    "dwrite.dll",
    "mswsock.dll",
    "windows.networking.dll",
    "fwpuclnt.dll",
    "userenv.dll",
    "comctl32.dll",
    "msvcp140.dll",
    "vcruntime140.dll",
    "vcruntime140_1.dll"
  )
  return $known -contains $name
}

function Resolve-VcRuntimeMissing {
  param([Parameter(Mandatory = $true)][string[]]$SearchPaths)
  $required = @("MSVCP140.dll", "VCRUNTIME140.dll", "VCRUNTIME140_1.dll")
  $missing = @()
  foreach ($dll in $required) {
    if (-not (Resolve-DllPath -DllName $dll -SearchPaths $SearchPaths)) {
      $missing += $dll
    }
  }
  return $missing
}

$visited = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
$queue = New-Object 'System.Collections.Generic.Queue[string]'
$queue.Enqueue($exePath)

while ($queue.Count -gt 0) {
  $binary = $queue.Dequeue()
  foreach ($dllName in (Get-DependentDllNames -BinaryPath $binary)) {
    if ($visited.Contains($dllName)) {
      continue
    }
    $visited.Add($dllName) | Out-Null
    $resolved = Resolve-DllPath -DllName $dllName -SearchPaths $searchPaths
    if ($null -eq $resolved) {
      if (-not (Test-SystemDll -DllName $dllName)) {
        Write-Warning "Missing DLL: $dllName (referenced by $(Split-Path -Leaf $binary))"
      }
      continue
    }
    Copy-Item $resolved -Destination $packageDir -Force
    $queue.Enqueue($resolved)
  }
}

$missingVcRuntime = Resolve-VcRuntimeMissing -SearchPaths $searchPaths
if ($missingVcRuntime.Count -gt 0) {
  Write-Warning ("Missing VC++ runtime DLLs: {0}. Install the Microsoft Visual C++ Redistributable (x64) or include the VC runtime DLLs alongside the exe." -f ($missingVcRuntime -join ", "))
}

$shareDir = Join-Path $packageDir "share"
New-Item -ItemType Directory -Path (Join-Path $shareDir "themes") | Out-Null
New-Item -ItemType Directory -Path (Join-Path $shareDir "gtk-4.0") | Out-Null
New-Item -ItemType Directory -Path (Join-Path $shareDir "glib-2.0") | Out-Null

$glibSchemas = Join-Path $gtkInstallPath "share\glib-2.0\schemas"
if (Test-Path $glibSchemas) {
  Copy-Item -Recurse $glibSchemas (Join-Path $shareDir "glib-2.0")
} else {
  Write-Warning "Missing GTK schemas directory: $glibSchemas"
}

$gtkIcons = Join-Path $gtkInstallPath "share\icons"
if (Test-Path $gtkIcons) {
  Copy-Item -Recurse $gtkIcons (Join-Path $shareDir "icons")
} else {
  Write-Warning "Missing GTK icons directory: $gtkIcons"
}

if (Test-Path $zipPath) {
  Remove-Item $zipPath -Force
}
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::CreateFromDirectory($packageDir, $zipPath)

} finally {
  if (Test-Path $tempManifestRoot) {
    try {
      Remove-Item -Recurse -Force $tempManifestRoot
    } catch {
      Write-Warning "Failed to delete temp vcpkg manifest directory: $tempManifestRoot"
    }
  }
}
