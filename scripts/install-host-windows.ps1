# Register the Grab native messaging host with Chrome on Windows.
#
# Usage (PowerShell):
#   .\scripts\install-host-windows.ps1 -ExtensionId <ID> [-HostBin <path-to-grab-host.exe>]
#
# If -HostBin is omitted, we use target\release\grab-host.exe (build it first
# with: cargo build --release -p grab-host).
#
# On Windows the host is registered via a registry key under HKCU that points
# to the manifest JSON file on disk.

param(
  [Parameter(Mandatory = $true)]
  [string]$ExtensionId,
  [string]$HostBin
)

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
if (-not $HostBin) {
  $HostBin = Join-Path $RepoRoot "target\release\grab-host.exe"
}

if (-not (Test-Path $HostBin)) {
  Write-Error "Host binary not found: $HostBin`nBuild it with: cargo build --release -p grab-host"
  exit 1
}
$HostBin = (Resolve-Path $HostBin).Path

$HostName = "com.grab.download_manager"
$Template = Join-Path $RepoRoot "host\manifest.template.json"

# Place the manifest next to the host binary (Chrome allows a relative path on
# Windows, but we use an absolute path for robustness).
$ManifestDir = Join-Path $env:LOCALAPPDATA "Grab"
New-Item -ItemType Directory -Force -Path $ManifestDir | Out-Null
$ManifestPath = Join-Path $ManifestDir "$HostName.json"

# JSON requires backslashes to be escaped.
$EscapedPath = $HostBin -replace '\\', '\\'

(Get-Content $Template -Raw) `
  -replace '__HOST_PATH__', $EscapedPath `
  -replace '__EXTENSION_ID__', $ExtensionId `
  | Set-Content -Path $ManifestPath -Encoding UTF8

Write-Host "Wrote manifest: $ManifestPath"

# Register the host for Chrome (and Chromium / Edge if desired).
$RegPaths = @(
  "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$HostName"
)
foreach ($reg in $RegPaths) {
  New-Item -Path $reg -Force | Out-Null
  Set-ItemProperty -Path $reg -Name "(default)" -Value $ManifestPath
  Write-Host "Registered: $reg"
}

Write-Host ""
Write-Host "Done. Restart Chrome, then try a download."
