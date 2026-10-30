# Builds and packages xara for Windows; mirror of release_bins.sh for the MSVC target.
param(
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$OutDir = ""
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
if (-not $OutDir) { $OutDir = Join-Path $root "target\dist" }
$ver = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version = "([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$name = "xara-$ver-$Target"
cargo build --locked --release --target $Target -p xara
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$rel = Join-Path $root "target\$Target\release"
New-Item -ItemType Directory -Force $OutDir | Out-Null
$stage = Join-Path ([System.IO.Path]::GetTempPath()) ("xara-stage-" + [guid]::NewGuid().ToString("N"))
$dir = Join-Path $stage $name
New-Item -ItemType Directory -Force $dir | Out-Null
try {
    Copy-Item (Join-Path $rel "xara.exe"), (Join-Path $rel "xara-monitor.exe"), (Join-Path $root "LICENSE"), (Join-Path $root "README.md") $dir
    Compress-Archive -Path $dir -DestinationPath (Join-Path $OutDir "$name.zip") -Force
} finally {
    Remove-Item -Recurse -Force $stage
}
Get-ChildItem $OutDir
