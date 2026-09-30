# Phase 6 checks that need Windows (WHPX) and the TempleOS ISO.
#
#   powershell -ExecutionPolicy Bypass -File tools\phase6-windows.ps1
#
# Needs: Rust (cargo), Python 3, qemu-system-x86_64 on PATH (8.2 is the
# reference version), Windows Hypervisor Platform enabled, and
# payload\TempleOS.ISO (tools/fetch-payload.sh, or copy it there).
#
# Runs, in order, and keeps going when one fails:
#   1. cargo build --release and the workspace tests
#   2. every tests\scripts\*.script on templeos.exe and on QEMU, and compares
#      the screenshots pixel for pixel (masked: the top line, and whatever
#      tests\scripts\NAME.masks lists as drawn from the time)
#   3. records ref\boot in QEMU and replays it through the board
# Everything lands in phase6-results\, with summary.txt listing pass/fail.

param(
    [int]$BootSeconds = 120
)

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$out = Join-Path $root 'phase6-results'
New-Item -ItemType Directory -Force $out | Out-Null
$summary = Join-Path $out 'summary.txt'
"Phase 6 run $(Get-Date -Format s) on $env:COMPUTERNAME" | Set-Content $summary
$failed = 0

function Step($name, [scriptblock]$body) {
    Write-Host "== $name" -ForegroundColor Cyan
    $log = Join-Path $out (($name -replace '[^\w.-]', '_') + '.log')
    & $body *>&1 | Tee-Object -FilePath $log
    $ok = $LASTEXITCODE -eq 0
    $line = '{0,-6} {1}   (log: {2})' -f ($(if ($ok) { 'PASS' } else { 'FAIL' })), $name, (Split-Path -Leaf $log)
    Add-Content $summary $line
    if (-not $ok) { $script:failed++ }
    Write-Host $line -ForegroundColor ($(if ($ok) { 'Green' } else { 'Red' }))
}

foreach ($tool in 'cargo', 'python', 'qemu-system-x86_64') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        Add-Content $summary "FAIL   $tool not found on PATH"
        Write-Host "$tool not found on PATH" -ForegroundColor Red
        exit 1
    }
}
if (-not (Test-Path payload\TempleOS.ISO)) {
    Add-Content $summary 'FAIL   payload\TempleOS.ISO missing'
    Write-Host 'payload\TempleOS.ISO missing' -ForegroundColor Red
    exit 1
}

# 1. Build and tests.
Step 'build' { cargo build --release }
Step 'tests' { cargo test --workspace }
$exe = Join-Path $root 'target\release\templeos.exe'

# 2. Scripted differential runs.
foreach ($script in Get-ChildItem tests\scripts\*.script) {
    $name = $script.BaseName
    $ours = Join-Path $out "$name\ours"
    $qemu = Join-Path $out "$name\qemu"
    Remove-Item -Recurse -Force $ours, $qemu -ErrorAction SilentlyContinue
    # One CPU on both sides: qemu_trace.py runs -smp 1, and the core count
    # changes what TempleOS allocates and so what it draws.
    Step "$name-templeos" { & $exe --no-hdd --cpus 1 --script $script.FullName --shots $ours }
    # Script waits are guest time on both machines (QEMU: its -icount clock,
    # which with sleep=on follows the host's while the guest idles, as
    # WHPX's does, so key gaps match too).
    Step "$name-qemu" { python tools\qemu-ref\qemu_trace.py --script $script.FullName --no-trace --icount-sleep --out $qemu }
    # tests\scripts\NAME.masks lists what the guest draws from the time.
    $masks = [IO.Path]::ChangeExtension($script.FullName, '.masks')
    $maskArgs = if (Test-Path $masks) { @('--masks', $masks) } else { @() }
    Step "$name-compare" { python tools\compare_shots.py $ours $qemu --mask 0:8 @maskArgs }
}

# 3. Real boot recorded in QEMU, replayed through the board.
Step 'ref-boot-record' { python tools\qemu-ref\qemu_trace.py --out ref\boot --seconds $BootSeconds }
Step 'ref-boot-replay' {
    $env:TEMPLEOS_TRACE = 'ref/boot/trace.log'
    cargo test -p devices --test replay_board -- --ignored --nocapture
    Remove-Item Env:TEMPLEOS_TRACE
}

Add-Content $summary "$failed failed"
Write-Host "`n$(Get-Content $summary -Raw)"
exit $(if ($failed) { 1 } else { 0 })
