# Build the release TempleOS.exe, reproducibly.
#
#   powershell -ExecutionPolicy Bypass -File tools\build-release.ps1 [-OutDir dist]
#
# Needs the MSVC Rust toolchain (rust-toolchain.toml pins the version) and
# payload\TempleOS.ISO (tools/fetch-payload.sh). Writes OutDir\TempleOS.exe,
# OutDir\TempleOS.pdb and OutDir\TempleOS.exe.sha256.
#
# The same sources and toolchain give a byte-identical exe on any machine and
# in any folder: build paths are remapped out of the binary (panic messages
# carry source paths), and build.rs makes the link deterministic (/Brepro,
# PDB referenced by file name only). CI checks this by building twice.

param(
    [string]$OutDir = 'dist',
    [string]$TargetDir = 'target'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$triple = 'x86_64-pc-windows-msvc'

$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
$cargoHome = [IO.Path]::GetFullPath($cargoHome)
# RUSTFLAGS replaces the rustflags in .cargo/config.toml, so repeat the static
# CRT here. The encoded form (0x1F between flags) survives spaces in paths.
$flags = @(
    '-C', 'target-feature=+crt-static',
    "--remap-path-prefix=$root=.",
    "--remap-path-prefix=$cargoHome=cargo"
)
$env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]0x1F
Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue

cargo build --release --locked --target $triple --target-dir $TargetDir
if ($LASTEXITCODE) { exit $LASTEXITCODE }
Remove-Item Env:CARGO_ENCODED_RUSTFLAGS

$built = Join-Path $TargetDir "$triple\release"
New-Item -ItemType Directory -Force $OutDir | Out-Null
Copy-Item (Join-Path $built 'templeos.exe') (Join-Path $OutDir 'TempleOS.exe')
if (Test-Path (Join-Path $built 'templeos.pdb')) {
    Copy-Item (Join-Path $built 'templeos.pdb') (Join-Path $OutDir 'TempleOS.pdb')
}
$exe = Join-Path $OutDir 'TempleOS.exe'

python tools\pe_imports.py $exe --check
if ($LASTEXITCODE) { exit $LASTEXITCODE }

$hash = (Get-FileHash -Algorithm SHA256 $exe).Hash.ToLower()
# sha256sum format, so `sha256sum -c TempleOS.exe.sha256` works.
"$hash *TempleOS.exe" | Out-File -Encoding ascii -NoNewline (Join-Path $OutDir 'TempleOS.exe.sha256')
Write-Host "$exe  sha256 $hash"
