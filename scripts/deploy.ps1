# Builds a release and installs it as dist\yusic.exe (what the desktop shortcut runs).
# A running Yusic is not interrupted: its exe is renamed aside (Windows allows that)
# and keeps playing; the next launch uses the new build. Yusic deletes the old copy
# on a later start.

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root 'dist'
$env:CARGO_TARGET_DIR = Join-Path $root 'target\work'

Push-Location $root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally {
    Pop-Location
}

New-Item -ItemType Directory -Force $dist | Out-Null
$exe = Join-Path $dist 'yusic.exe'
if (Test-Path $exe) {
    $aside = Join-Path $dist ("yusic.old-{0}.exe" -f (Get-Date -Format 'yyyyMMddHHmmss'))
    Move-Item $exe $aside
}
Copy-Item (Join-Path $env:CARGO_TARGET_DIR 'release\yusic.exe') $exe

# Clean up old copies that are no longer running.
Get-ChildItem $dist -Filter 'yusic.old*.exe' | ForEach-Object {
    try { Remove-Item $_.FullName -ErrorAction Stop } catch { }
}
Write-Host "Installed $exe ($((Get-Item $exe).LastWriteTime))"
