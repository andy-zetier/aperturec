#
# Windows native packaging script (creates ZIP + MSI and signs MSI).
#
# CI environment / local use:
# - GitHub-hosted Windows runner label `windows-2025`
#   https://github.com/actions/runner-images/blob/main/images/windows/Windows2025-Readme.md
# - Can also be run on local Windows hosts with PowerShell
#
# Dependencies:
# - winget
# - Windows SDK (signtool.exe)
# - Visual Studio Build Tools (dumpbin.exe)
#
# Environment variables
# ---------------------
# Required:
# - AZURE_APP_ID
# - AZURE_APP_SECRET
# - AZURE_TENANT_ID
# - AZURE_CERT_ALIAS
#
# Optional overrides:
# - VCPKG_INSTALLED_DIR: vcpkg installed directory from the build step. If set and exists,
#   vcpkg root checks are skipped and DLL resolution uses this path.
#   Expected triplets: x64-windows, x64-windows-static.
# - VCPKG_ROOT: vcpkg install root; only required if VCPKG_INSTALLED_DIR is missing.
# - GTK_ROOT: GTK install root. If unset, auto-resolved via GVSBUILD_* or default path.
# - WIX / WIXROOT: WiX Toolset v3 root; if unset, the script searches standard install paths.
#
# Set by this script:
# - VCPKG_INSTALLED_DIR, AZURE_TENANT_ID, AZURE_CLIENT_ID, AZURE_CLIENT_SECRET.
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$VcpkgRoot = $env:VCPKG_ROOT

$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Resolve-Path (Join-Path $scriptRoot "..\..")
$clientTomlPath = Join-Path $repoRoot "aperturec-client-gtk4\Cargo.toml"

function Test-GtkRoot {
  param([string]$Root)
  if (-not $Root) {
    return $false
  }
  $gtk4Dll = Join-Path $Root "bin\gtk-4-1.dll"
  return (Test-Path $gtk4Dll)
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

function Get-DependentDllNames {
  param(
    [Parameter(Mandatory = $true)][string]$BinaryPath,
    [Parameter(Mandatory = $true)][string]$DumpbinPath
  )
  $lines = & $DumpbinPath /dependents $BinaryPath 2>$null
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
    "advapi32.dll",
    "bcrypt.dll",
    "bcryptprimitives.dll",
    "combase.dll",
    "comctl32.dll",
    "comdlg32.dll",
    "crypt32.dll",
    "d3d11.dll",
    "d3d12.dll",
    "dcomp.dll",
    "dnsapi.dll",
    "dwmapi.dll",
    "dwrite.dll",
    "dxgi.dll",
    "fwpuclnt.dll",
    "gdi32.dll",
    "hid.dll",
    "imm32.dll",
    "iphlpapi.dll",
    "kernel32.dll",
    "msimg32.dll",
    "msvcp140.dll",
    "mswsock.dll",
    "ntdll.dll",
    "ole32.dll",
    "oleaut32.dll",
    "opengl32.dll",
    "pdh.dll",
    "powrprof.dll",
    "psapi.dll",
    "rpcrt4.dll",
    "secur32.dll",
    "setupapi.dll",
    "shell32.dll",
    "shlwapi.dll",
    "user32.dll",
    "userenv.dll",
    "usp10.dll",
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    "windows.networking.dll",
    "winmm.dll",
    "ws2_32.dll"
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

function Require-Command {
  param(
    [Parameter(Mandatory = $true)][string]$Name,
    [Parameter(Mandatory = $true)][string]$Hint
  )
  if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
    throw "$Name not found. $Hint"
  }
}

function Add-PathEntry {
  param([Parameter(Mandatory = $true)][string]$PathEntry)
  if ($PathEntry -and (Test-Path $PathEntry) -and ($env:Path -notlike "*$PathEntry*")) {
    $env:Path = "$PathEntry;$env:Path"
  }
}

if (-not $VcpkgRoot) {
  $VcpkgRoot = Resolve-VcpkgRoot
}

Write-Host "Resolved VCPKG_ROOT=$VcpkgRoot"

if ($env:GTK_ROOT -and -not (Test-Path $env:GTK_ROOT)) {
  throw "GTK root not found at $env:GTK_ROOT (from GTK_ROOT). Update/unset GTK_ROOT or create that directory."
}

$GtkRoot = Resolve-GtkRoot -PreferredRoot $env:GTK_ROOT
if (-not $GtkRoot) {
  if ($env:GTK_ROOT) {
    throw "GTK root not found at $env:GTK_ROOT (from GTK_ROOT). Update/unset GTK_ROOT or ensure gvsbuild installed to a standard location."
  }
  throw "GTK root not found (auto-resolved). Set GTK_ROOT or ensure gvsbuild installed to a standard location."
}

$vcpkgInstalledDir = if ($env:VCPKG_INSTALLED_DIR) { $env:VCPKG_INSTALLED_DIR } else { Join-Path $repoRoot "vcpkg_installed" }
$env:VCPKG_INSTALLED_DIR = $vcpkgInstalledDir
Write-Host "Set VCPKG_INSTALLED_DIR=$env:VCPKG_INSTALLED_DIR"

if (-not (Test-Path $vcpkgInstalledDir)) {
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
}

function Get-ProjectVersion {
  param([Parameter(Mandatory = $true)][string]$TomlPath)
  if (-not (Test-Path $TomlPath)) {
    throw "Cargo.toml not found at $TomlPath"
  }
  $content = Get-Content -Raw -Path $TomlPath
  $match = [regex]::Match($content, '^\s*version\s*=\s*"([^"]+)"\s*$', 'Multiline')
  if (-not $match.Success) {
    throw "Unable to parse version from $TomlPath"
  }
  return $match.Groups[1].Value
}

function Refresh-PathFromRegistry {
  $machinePath = (Get-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment" -Name Path -ErrorAction SilentlyContinue).Path
  $userPath = (Get-ItemProperty -Path "HKCU:\Environment" -Name Path -ErrorAction SilentlyContinue).Path
  $combined = @($machinePath, $userPath) | Where-Object { $_ -and $_ -ne "" }
  if ($combined.Count -gt 0) {
    $env:Path = ($combined -join ";")
  }
}

function Resolve-Tool {
  param(
    [Parameter(Mandatory = $true)][string]$DisplayName,
    [Parameter(Mandatory = $true)][string]$CommandName,
    [Parameter()][string]$WingetId,
    [Parameter()][string[]]$KnownPaths = @()
  )

  $cmd = Get-Command $CommandName -ErrorAction SilentlyContinue
  if ($cmd) {
    return $cmd.Source
  }

  foreach ($path in $KnownPaths) {
    if ($path -and (Test-Path $path)) {
      return $path
    }
  }

  if ($WingetId -and (Get-Command winget -ErrorAction SilentlyContinue)) {
    # winget installs may require administrator privileges if running locally
    Write-Host "$DisplayName not found; attempting winget install ($DisplayName)"
    try {
      $wingetOutput = & winget install --id $WingetId -e --silent --accept-source-agreements --accept-package-agreements 2>&1
      if ($wingetOutput) {
        $wingetOutput | ForEach-Object { Write-Host $_ }
      }
      if ($LASTEXITCODE -ne 0) {
        Write-Warning "winget install failed with exit code $LASTEXITCODE"
      }
    } catch {
      Write-Warning "winget install failed: $($_.Exception.Message)"
    }
    Refresh-PathFromRegistry
  }

  $cmd = Get-Command $CommandName -ErrorAction SilentlyContinue
  if ($cmd) {
    return $cmd.Source
  }

  foreach ($path in $KnownPaths) {
    if ($path -and (Test-Path $path)) {
      return $path
    }
  }

  return $null
}

function Resolve-WixBin {
  $tools = @("heat.exe", "candle.exe", "light.exe")
  $onPath = $true
  foreach ($tool in $tools) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
      $onPath = $false
      break
    }
  }
  if ($onPath) {
    $cmd = Get-Command "candle.exe" -ErrorAction SilentlyContinue
    if ($cmd -and -not [string]::IsNullOrWhiteSpace($cmd.Source)) {
      $bin = Split-Path -Parent $cmd.Source
      if (-not [string]::IsNullOrWhiteSpace($bin)) {
        return $bin
      }
    }
  }

  $candidates = @()
  if (-not [string]::IsNullOrWhiteSpace($env:WIX)) { $candidates += (Join-Path $env:WIX "bin") }
  if (-not [string]::IsNullOrWhiteSpace($env:WIXROOT)) { $candidates += (Join-Path $env:WIXROOT "bin") }
  $candidates += @(
    "C:\Program Files (x86)\WiX Toolset v3.11\bin",
    "C:\Program Files\WiX Toolset v3.11\bin",
    "C:\Program Files (x86)\WiX Toolset v3.14\bin",
    "C:\Program Files\WiX Toolset v3.14\bin"
  )
  foreach ($root in @("C:\Program Files (x86)", "C:\Program Files")) {
    if (Test-Path $root) {
      $dynamic = Get-ChildItem -Path $root -Directory -Filter "WiX Toolset v3*" -ErrorAction SilentlyContinue
      foreach ($dir in $dynamic) {
        $candidates += (Join-Path $dir.FullName "bin")
      }
    }
  }
  foreach ($candidate in $candidates) {
    if (-not (Test-Path $candidate)) {
      continue
    }
    $ok = $true
    foreach ($tool in $tools) {
      if (-not (Test-Path (Join-Path $candidate $tool))) {
        $ok = $false
        break
      }
    }
    if ($ok) {
      return $candidate
    }
  }

  if (Get-Command winget -ErrorAction SilentlyContinue) {
    # winget installs may require administrator privileges.
    Write-Host "WiX not found; attempting winget install (WiX Toolset)"
    try {
      $wingetOutput = & winget install --id WiXToolset.WiXToolset -e --silent --accept-source-agreements --accept-package-agreements 2>&1
      if ($wingetOutput) {
        $wingetOutput | ForEach-Object { Write-Host $_ }
      }
      if ($LASTEXITCODE -ne 0) {
        Write-Warning "winget install failed with exit code $LASTEXITCODE"
      }
    } catch {
      Write-Warning "winget install failed: $($_.Exception.Message)"
    }
    Refresh-PathFromRegistry
    foreach ($candidate in $candidates) {
      if (-not (Test-Path $candidate)) {
        continue
      }
      $ok = $true
      foreach ($tool in $tools) {
        if (-not (Test-Path (Join-Path $candidate $tool))) {
          $ok = $false
          break
        }
      }
      if ($ok) {
        return $candidate
      }
    }
  }

  return $null
}

function Resolve-Pandoc {
  $known = @(
    (Join-Path $env:ProgramFiles "Pandoc\pandoc.exe"),
    (Join-Path $env:LOCALAPPDATA "Pandoc\pandoc.exe")
  )
  return Resolve-Tool -DisplayName "Pandoc" -CommandName "pandoc" -WingetId "JohnMacFarlane.Pandoc" -KnownPaths $known
}

function Resolve-SignTool {
  $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
  if ($cmd) {
    return $cmd.Source
  }

  # Windows SDK installs (which include signtool.exe) typically require administrator privileges.
  $kitRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
  if (Test-Path $kitRoot) {
    $versions = Get-ChildItem -Path $kitRoot -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending
    foreach ($version in $versions) {
      $candidate = Join-Path $version.FullName "x64\signtool.exe"
      if (Test-Path $candidate) {
        return $candidate
      }
    }
  }

  $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
  if (Test-Path $vswhere) {
    $installPath = & $vswhere -latest -products * -property installationPath
    if ($installPath) {
      $sdkBin = Join-Path $installPath "Windows Kits\10\bin"
      if (Test-Path $sdkBin) {
        $versions = Get-ChildItem -Path $sdkBin -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending
        foreach ($version in $versions) {
          $candidate = Join-Path $version.FullName "x64\signtool.exe"
          if (Test-Path $candidate) {
            return $candidate
          }
        }
      }
    }
  }

  return $null
}

function Resolve-AzCli {
  $known = @(
    (Join-Path $env:ProgramFiles "Microsoft SDKs\Azure\CLI2\wbin\az.cmd")
  )
  return Resolve-Tool -DisplayName "Microsoft Azure CLI" -CommandName "az" -WingetId "Microsoft.AzureCLI" -KnownPaths $known
}

function Resolve-TrustedSigningDlib {
  $candidates = @(
    (Join-Path $env:ProgramFiles "Microsoft\Azure\Trusted Signing Client Tools\Azure.CodeSigning.Dlib.dll"),
    (Join-Path ${env:ProgramFiles(x86)} "Microsoft\Azure\Trusted Signing Client Tools\Azure.CodeSigning.Dlib.dll"),
    (Join-Path $env:ProgramFiles "Microsoft Azure Trusted Signing Client Tools\Azure.CodeSigning.Dlib.dll"),
    (Join-Path ${env:ProgramFiles(x86)} "Microsoft Azure Trusted Signing Client Tools\Azure.CodeSigning.Dlib.dll"),
    (Join-Path $env:LOCALAPPDATA "Microsoft\MicrosoftTrustedSigningClientTools\Azure.CodeSigning.Dlib.dll")
  ) | Where-Object { $_ -and (Test-Path $_) }

  $candidates = @($candidates)
  if ($candidates.Count -gt 0) {
    return $candidates[0]
  }

  if (Get-Command winget -ErrorAction SilentlyContinue) {
    # winget installs may require administrator privileges.
    Write-Host "Trusted Signing Client Tools not found; attempting winget install"
    try {
      $wingetOutput = & winget install --id Microsoft.Azure.TrustedSigningClientTools -e --silent --accept-source-agreements --accept-package-agreements 2>&1
      if ($wingetOutput) {
        $wingetOutput | ForEach-Object { Write-Host $_ }
      }
      if ($LASTEXITCODE -ne 0) {
        Write-Warning "winget install failed with exit code $LASTEXITCODE"
      }
    } catch {
      Write-Warning "winget install failed: $($_.Exception.Message)"
    }
    Refresh-PathFromRegistry
  }

  $candidates = @(
    (Join-Path $env:ProgramFiles "Microsoft\Azure\Trusted Signing Client Tools\Azure.CodeSigning.Dlib.dll"),
    (Join-Path ${env:ProgramFiles(x86)} "Microsoft\Azure\Trusted Signing Client Tools\Azure.CodeSigning.Dlib.dll"),
    (Join-Path $env:ProgramFiles "Microsoft Azure Trusted Signing Client Tools\Azure.CodeSigning.Dlib.dll"),
    (Join-Path ${env:ProgramFiles(x86)} "Microsoft Azure Trusted Signing Client Tools\Azure.CodeSigning.Dlib.dll"),
    (Join-Path $env:LOCALAPPDATA "Microsoft\MicrosoftTrustedSigningClientTools\Azure.CodeSigning.Dlib.dll")
  ) | Where-Object { $_ -and (Test-Path $_) }

  $candidates = @($candidates)
  if ($candidates.Count -gt 0) {
    return $candidates[0]
  }

  $roots = @($env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:LOCALAPPDATA) | Where-Object { $_ -and (Test-Path $_) }
  foreach ($root in $roots) {
    try {
      $found = Get-ChildItem -Path $root -Recurse -Filter "Azure.CodeSigning.Dlib.dll" -ErrorAction SilentlyContinue -Depth 4 | Select-Object -First 1
      if ($found) {
        return $found.FullName
      }
    } catch {
      # Ignore recursive scan errors
    }
  }

  return $null
}

#
# Verify required tools are installed and on PATH.
#
$pandocExe = Resolve-Pandoc
if (-not $pandocExe) {
  throw "pandoc not found. Install pandoc and ensure it is on PATH."
}
$wixBin = [string](Resolve-WixBin)
if ([string]::IsNullOrWhiteSpace($wixBin) -or -not (Test-Path $wixBin)) {
  throw "WiX Toolset v3 not found. Install WiX v3 (heat/candle/light) or ensure it is on PATH."
}
$signTool = Resolve-SignTool
if (-not $signTool) {
  throw "signtool.exe not found. Install the Windows SDK or ensure signtool.exe is on PATH."
}
$azCli = Resolve-AzCli
if (-not $azCli) {
  throw "Azure CLI (az) not found. Install Azure CLI to sign with Azure Trusted Signing."
}
$dumpbinExe = $null
$dumpbinCmd = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
if ($null -ne $dumpbinCmd) {
  $dumpbinExe = $dumpbinCmd.Source
} else {
  $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
  if (Test-Path $vswhere) {
    $installPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($installPath) {
      $msvcRoot = Join-Path $installPath "VC\Tools\MSVC"
      $versions = Get-ChildItem -Path $msvcRoot -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending
      foreach ($version in $versions) {
        $candidate = Join-Path $version.FullName "bin\Hostx64\x64\dumpbin.exe"
        if (Test-Path $candidate) {
          $dumpbinExe = $candidate
          break
        }
        $candidate = Join-Path $version.FullName "bin\Hostx86\x64\dumpbin.exe"
        if (Test-Path $candidate) {
          $dumpbinExe = $candidate
          break
        }
      }
    }
  }
}
if (-not $dumpbinExe) {
  throw "dumpbin.exe not found (ensure VS Build Tools are installed and on PATH)"
}

Add-PathEntry -PathEntry (Split-Path -Parent $pandocExe)
Add-PathEntry -PathEntry $wixBin
Add-PathEntry -PathEntry (Split-Path -Parent $signTool)
Add-PathEntry -PathEntry (Split-Path -Parent $azCli)
Add-PathEntry -PathEntry (Split-Path -Parent $dumpbinExe)

Require-Command -Name "pandoc" -Hint "Install pandoc and ensure it is on PATH."
Require-Command -Name "heat.exe" -Hint "Install WiX Toolset v3 and ensure it is on PATH."
Require-Command -Name "candle.exe" -Hint "Install WiX Toolset v3 and ensure it is on PATH."
Require-Command -Name "light.exe" -Hint "Install WiX Toolset v3 and ensure it is on PATH."
Require-Command -Name "signtool.exe" -Hint "Install the Windows SDK and ensure signtool.exe is on PATH."
Require-Command -Name "az" -Hint "Install the Azure CLI and ensure it is on PATH."
Require-Command -Name "dumpbin.exe" -Hint "Install Visual Studio Build Tools and ensure dumpbin.exe is on PATH."

#
# Package the exe, DLL dependencies, and GTK assets into a zip.
#
Write-Host "==> Package ApertureC"
$packageDir = Join-Path $repoRoot "package"
$zipPath = Join-Path $repoRoot "aperturec-client-win.zip"
$targetDir = Join-Path $repoRoot "target\x86_64-pc-windows-msvc\release"
$exePath = Join-Path $targetDir "aperturec-client-gtk4.exe"
$packagedExeName = "aperturec-client.exe"
$gtkInstallPath = $GtkRoot
$searchPaths = @(
  $targetDir,
  (Join-Path $gtkInstallPath "bin"),
  (Join-Path $vcpkgInstalledDir "x64-windows\bin"),
  (Join-Path $vcpkgInstalledDir "x64-windows-static\bin")
)

if (Test-Path $packageDir) {
  throw "Package directory already exists at $packageDir. Remove it before packaging."
}
New-Item -ItemType Directory -Path $packageDir | Out-Null

Copy-Item $exePath -Destination (Join-Path $packageDir $packagedExeName)

$visited = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
$queue = New-Object 'System.Collections.Generic.Queue[string]'
$queue.Enqueue($exePath)

while ($queue.Count -gt 0) {
  $binary = $queue.Dequeue()
  foreach ($dllName in (Get-DependentDllNames -BinaryPath $binary -DumpbinPath $dumpbinExe)) {
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

#
# Create the zip archive.
#
if (Test-Path $zipPath) {
  throw "Zip already exists at $zipPath. Remove it before packaging."
}

#
# Generate License.rtf for MSI.
#
# Include License.rtf for MSI (generated from LICENSE).
$licensePath = Join-Path $repoRoot "LICENSE"
$licenseRtf = Join-Path $packageDir "License.rtf"
if (-not (Test-Path $licensePath)) {
  throw "LICENSE not found at $licensePath"
}
& $pandocExe -s -f markdown -t rtf -o $licenseRtf $licensePath
if ($LASTEXITCODE -ne 0) {
  throw "pandoc failed with exit code $LASTEXITCODE"
}

#
# Create the zip archive after staging is complete.
#
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::CreateFromDirectory($packageDir, $zipPath)
Write-Host ("Zip created at: {0}" -f $zipPath)

#
# Build MSI with WiX v3 (no signing yet).
#
$version = Get-ProjectVersion -TomlPath $clientTomlPath
Write-Host ("==> Build MSI (version {0})" -f $version)
if ($env:Path -notlike "*$wixBin*") {
  $env:Path = "$wixBin;$env:Path"
}

#
# Generate WiX fragments and build the MSI.
#
$wixTempDir = Join-Path $env:TEMP ("aperturec-wix-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $wixTempDir | Out-Null

$filesWxs = Join-Path $wixTempDir "files.wxs"
$packageWxsSource = Join-Path $repoRoot "ci\package.wxs"
$packageWxs = Join-Path $wixTempDir "package.wxs"
$sourceDir = "$packageDir\"
$msiPath = Join-Path $repoRoot ("aperturec-client_{0}.msi" -f $version)
$heatExe = Join-Path $wixBin "heat.exe"
$candleExe = Join-Path $wixBin "candle.exe"
$lightExe = Join-Path $wixBin "light.exe"

if (Test-Path $msiPath) {
  throw "MSI already exists at $msiPath. Remove it before packaging."
}

try {
  if (-not (Test-Path $packageWxsSource)) {
    throw "package.wxs not found at $packageWxsSource"
  }
  $packageWxsContent = Get-Content -Raw -Path $packageWxsSource
  $packageWxsContent = $packageWxsContent -replace 'Name=""', ''
  $packageWxsContent = $packageWxsContent -replace '\$\(\s*var\.Version\s*\)', $version
  if ($packageWxsContent -notmatch 'WixUILicenseRtf') {
    $licenseVar = '    <WixVariable Id="WixUILicenseRtf" Value="$(var.SourceDir)License.rtf"/>'
    $packageWxsContent = [regex]::Replace(
      $packageWxsContent,
      '(<UIRef[^>]+/>)',
      { param($m) $m.Value + "`r`n" + $licenseVar }
    )
  }
  $packageWxsContent = [regex]::Replace(
    $packageWxsContent,
    '\$\(\s*var\.SourceDir\s*\)',
    { $sourceDir }
  )
  $packageWxsContent | Set-Content -Path $packageWxs -NoNewline

  & $heatExe dir $packageDir -cg ClientFiles -dr INSTALLFOLDER -var var.SourceDir -gg -srd -sfrag -sreg -out $filesWxs
  if ($LASTEXITCODE -ne 0) {
    throw "heat.exe failed with exit code $LASTEXITCODE"
  }

  $filesWxsContent = Get-Content -Raw -Path $filesWxs
  $filesWxsContent = [regex]::Replace(
    $filesWxsContent,
    '\$\(\s*var\.SourceDir\s*\)',
    { $sourceDir }
  )
  $filesWxsContent | Set-Content -Path $filesWxs -NoNewline

  & $candleExe -arch x64 -dSourceDir=$sourceDir -dWin64=yes -dVersion=$version -out "$wixTempDir\" $packageWxs $filesWxs
  if ($LASTEXITCODE -ne 0) {
    throw "candle.exe failed with exit code $LASTEXITCODE"
  }

  & $lightExe -ext WixUIExtension -sice:ICE27 -sice:ICE61 -out $msiPath (Join-Path $wixTempDir "package.wixobj") (Join-Path $wixTempDir "files.wixobj")
  if ($LASTEXITCODE -ne 0) {
    throw "light.exe failed with exit code $LASTEXITCODE"
  }
} finally {
  if (Test-Path $wixTempDir) {
    try {
      Remove-Item -Recurse -Force $wixTempDir
    } catch {
      Write-Warning "Failed to delete temp WiX directory: $wixTempDir"
    }
  }
}

Write-Host ("MSI created at: {0}" -f $msiPath)

#
# Sign MSI with Azure Trusted Signing via SignTool.exe.
#

# The following environment variables are GitHub secrets:
#   AZURE_APP_ID
#   AZURE_APP_SECRET
#   AZURE_TENANT_ID
#   AZURE_CERT_ALIAS

$azureAppId = $env:AZURE_APP_ID
$azureAppSecret = $env:AZURE_APP_SECRET
$azureTenantId = $env:AZURE_TENANT_ID
$azureCertAlias = $env:AZURE_CERT_ALIAS
$trustedSigningAccount = "ApertureC"
$trustedSigningEndpoint = "https://eus.codesigning.azure.net"

if (-not $azureAppId -or -not $azureAppSecret -or -not $azureTenantId -or -not $azureCertAlias) {
  throw "Azure signing environment variables are not fully set. Require AZURE_APP_ID, AZURE_APP_SECRET, AZURE_TENANT_ID, AZURE_CERT_ALIAS."
}

Write-Host "==> Sign MSI with Azure Trusted Signing"
Write-Host "Using Azure tenant: $azureTenantId"
Write-Host "Using Trusted Signing account: $trustedSigningAccount"
Write-Host "Using Trusted Signing endpoint: $trustedSigningEndpoint"
try {
  $azVersion = & $azCli --version 2>&1
  if ($azVersion) {
    $azVersion | ForEach-Object { Write-Host $_ }
  }
} catch {
  Write-Warning "Failed to read Azure CLI version: $($_.Exception.Message)"
}
& $azCli login --service-principal --username $azureAppId --password $azureAppSecret --tenant $azureTenantId | Out-Null
if ($LASTEXITCODE -ne 0) {
  throw "az login failed with exit code $LASTEXITCODE"
}

$env:AZURE_TENANT_ID = $azureTenantId
$env:AZURE_CLIENT_ID = $azureAppId
$env:AZURE_CLIENT_SECRET = $azureAppSecret

# SignTool uses the Trusted Signing dlib and metadata JSON.
$dlibPath = Resolve-TrustedSigningDlib
if (-not $dlibPath) {
  throw "Trusted Signing Client Tools not found. Install Microsoft.Azure.TrustedSigningClientTools."
}

$signMetadata = @{
  Endpoint = $trustedSigningEndpoint
  CodeSigningAccountName = $trustedSigningAccount
  CertificateProfileName = $azureCertAlias
}
$signMetaPath = Join-Path $env:TEMP ("aperturec-sign-metadata-" + [Guid]::NewGuid().ToString("N") + ".json")
($signMetadata | ConvertTo-Json -Depth 3) | Set-Content -Path $signMetaPath -NoNewline

$signtoolArgs = @(
  "sign",
  "/v",
  "/debug",
  "/fd", "SHA256",
  "/td", "SHA256",
  "/tr", "http://timestamp.digicert.com",
  "/d", "ApertureC Client",
  "/du", "www.zetier.com",
  "/dlib", $dlibPath,
  "/dmdf", $signMetaPath,
  $msiPath
)

$signtoolArgs | ForEach-Object { Write-Host ("signtool arg: {0}" -f $_) }
& $signTool @signtoolArgs
if ($LASTEXITCODE -ne 0) {
  throw "signtool.exe failed with exit code $LASTEXITCODE"
}
