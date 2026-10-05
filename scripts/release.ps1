# Builds the public Yusic.exe and publishes it as a GitHub release.
#
#   scripts\release.ps1 -Version 0.2.0 [-Notes "What changed"] [-DryRun]
#
# Local paths (user profile, cargo registry, source folder) are rewritten in the
# binary so the public exe doesn't contain them. The version must match
# Cargo.toml; installed copies update themselves to the newest release.

param(
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$Notes = "",
    [switch]$DryRun
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $cargoVersion = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
    if ($cargoVersion -ne $Version) { throw "Cargo.toml says $cargoVersion, not $Version" }

    $sep = [char]0x1f
    $maps = @(
        "--remap-path-prefix=$env:USERPROFILE\.cargo\registry\src=crates",
        "--remap-path-prefix=$env:USERPROFILE\.cargo=cargo",
        "--remap-path-prefix=$env:USERPROFILE\.rustup=rustup",
        "--remap-path-prefix=$env:USERPROFILE=home",
        "--remap-path-prefix=$root=yusic"
    )
    $env:CARGO_ENCODED_RUSTFLAGS = $maps -join $sep
    $env:CARGO_TARGET_DIR = Join-Path $root 'target\publish'

    $p = Start-Process cargo -ArgumentList 'build', '--release', '-j', '4' -NoNewWindow -PassThru
    $null = $p.Handle
    try { $p.PriorityClass = 'BelowNormal' } catch { }
    $p.WaitForExit()
    if ($p.ExitCode -ne 0) { throw "build failed" }

    $built = Join-Path $env:CARGO_TARGET_DIR 'release\yusic.exe'
    $text = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($built))
    $user = Split-Path $env:USERPROFILE -Leaf
    if ($text.Contains("\Users\$user\")) { throw "binary still contains the user profile path" }
    if ($text.Contains($root)) { throw "binary still contains the source folder path" }

    $out = Join-Path $root 'target\publish\Yusic.exe'
    Copy-Item $built $out -Force
    $hash = (Get-FileHash $out -Algorithm SHA256).Hash.ToLower()
    Write-Host "Built $out"
    Write-Host "SHA-256 $hash"

    if ($DryRun) { return }
    if (-not $Notes) { $Notes = "Yusic $Version" }
    $Notes += "`n`nSHA-256 of Yusic.exe: ``$hash``"
    gh release create "v$Version" $out --title "Yusic $Version" --notes $Notes
    if ($LASTEXITCODE -ne 0) { throw "gh release create failed" }
} finally {
    Pop-Location
}
