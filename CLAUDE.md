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

`tools\phase6-windows.ps1` passes on the user's PC (2026-09-30: 10/10).
What's left needs a person at the window; when a session starts on the
user's Windows machine, remind them of it first:

1. In the TempleOS window, with the real keyboard and mouse: type in the
   shell, click to capture the mouse, check the PC speaker is audible (e.g.
   a hymn).
2. Run demos by hand, then install to the hard disk (BootHDIns) and boot
   from the disk.

When both are done, tick the In-guest tests item in PLAN.md's Phase 6 (and
the Phase 4 "still to check by hand" note) and delete this section.
