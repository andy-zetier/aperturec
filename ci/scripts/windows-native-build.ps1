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
cargo build --release -p aperturec-client-gtk4 --target x86_64-pc-windows-msvc

Write-Host "==> Package ApertureC"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Resolve-Path (Join-Path $scriptRoot "..\..")
$packageDir = Join-Path $repoRoot "package"
$zipPath = Join-Path $repoRoot "aperturec-client-win.zip"
$targetDir = Join-Path $repoRoot "target\x86_64-pc-windows-msvc\release"
$exePath = Join-Path $targetDir "aperturec-client.exe"
$gtkInstallPath = "C:\gtk-build\gtk\x64\release"
$searchPaths = @(
  $targetDir,
  (Join-Path $gtkInstallPath "bin"),
  "C:\vcpkg\installed\x64-windows\bin",
  "C:\vcpkg\installed\x64-windows-static\bin"
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
      Write-Warning "Missing DLL: $dllName (referenced by $(Split-Path -Leaf $binary))"
      continue
    }
    Copy-Item $resolved -Destination $packageDir -Force
    $queue.Enqueue($resolved)
  }
}

$shareDir = Join-Path $packageDir "share"
New-Item -ItemType Directory -Path (Join-Path $shareDir "themes") | Out-Null
New-Item -ItemType Directory -Path (Join-Path $shareDir "gtk-4.0") | Out-Null
New-Item -ItemType Directory -Path (Join-Path $shareDir "glib-2.0") | Out-Null

Copy-Item -Recurse (Join-Path $gtkInstallPath "share\glib-2.0\schemas") (Join-Path $shareDir "glib-2.0")
Copy-Item -Recurse (Join-Path $gtkInstallPath "share\icons") (Join-Path $shareDir "icons")

if (Test-Path $zipPath) {
  Remove-Item $zipPath -Force
}
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::CreateFromDirectory($packageDir, $zipPath)
