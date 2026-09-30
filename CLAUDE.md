# TempleOS.exe

TempleOS V5.03 (the official ISO, unmodified) in one Windows exe, on a
purpose-built VMM over the Windows Hypervisor Platform (WHPX). QEMU 8.2 is
the reference machine: device models must match it. PLAN.md has the design
and the phase status; docs/hw-surface.md is the device spec.

- `devices/`: the board (PIC, PIT, RTC, PS/2, VGA, IDE, PCI, HPET, ...), pure
  Rust, tested on any OS. `vmm/`: WHPX partition and vCPU loop (Windows).
  `src/`: the exe (window, input, audio).
- Tests: `cargo test --workspace`. Replays against QEMU traces are
  `--ignored`; see tools/qemu-ref/README.md.
- Release build: `tools/build-release.ps1` (reproducible; CI checks it).
- `payload/TempleOS.ISO` is not committed: `tools/fetch-payload.sh`.
- Develop on branch `claude/templeos-windows-binary-rlrv6x`.

## Pending on the user's Windows PC — remind them at the start of a session

The Phase 6 checks that need WHPX and the ISO have not been run yet. When a
session starts on the user's Windows machine, remind them of this first and
offer to run it:

1. `git pull origin claude/templeos-windows-binary-rlrv6x`
2. Put the ISO in `payload\TempleOS.ISO` (or `bash tools/fetch-payload.sh`).
3. Needs Rust, Python 3 and `qemu-system-x86_64` on PATH, and the Windows
   Hypervisor Platform turned on.
4. `powershell -ExecutionPolicy Bypass -File tools\phase6-windows.ps1`
5. Read `phase6-results\summary.txt`; for each FAIL, read its log (and the
   differing screenshots under `phase6-results\<script>\`) and fix the cause.
6. By hand: run the demos in TempleOS and install to the hard disk
   (BootHDIns), then boot from the disk.

When all of it passes, mark the Phase 6 items in PLAN.md done and delete
this section.
