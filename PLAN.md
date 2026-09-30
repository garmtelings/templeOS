# TempleOS as a single Windows binary — plan

## Goal

One file, `TempleOS.exe`. Double-click it and TempleOS boots in a window.
No installer, no QEMU or VirtualBox install, no loose ISO next to it.

## What "byte-instruction perfect" means here

1. **Unmodified guest.** The exe runs the official TempleOS V5.03 release
   image byte for byte: no patched kernel, no recompiled HolyC, no
   "userspace port". The embedded ISO is checked against the published
   SHA-256 at build time and again at startup.
2. **Every guest instruction runs with real x86-64 semantics.** Guest code
   runs on the host CPU through the Windows Hypervisor Platform (WHPX), so
   the processor is the reference implementation. We never translate or
   interpret guest instructions on the main path.
3. **The emulated hardware matches real PC hardware for everything TempleOS
   touches.** The virtual devices must give the same observable results
   (port reads, IRQ timing order, framebuffer contents) as QEMU and real
   hardware. A differential test harness checks this (see Phase 6).

Native ports such as EXODUS, ZealOS or TinkerOS fail rule 1: they change the
kernel to run in ring 3. That's why this plan uses a small purpose-built VMM.

## Architecture

```
TempleOS.exe (PE32+, x86-64, statically linked, no DLL deps beyond Windows)
├── .rsrc / appended payload
│   ├── TempleOS.ISO        (official V5.03, SHA-256 pinned)
│   ├── bios.bin            (SeaBIOS, pinned build)
│   └── vgabios.bin         (SeaVGABIOS, pinned build)
├── VMM core   — WHPX partition, vCPUs, guest RAM, exit dispatch
├── Devices    — only what TempleOS actually uses (list below)
├── Frontend   — Win32 window + D3D11/GDI blit, keyboard/mouse capture, audio
└── Storage    — ISO read-only from memory; HDD image as a sidecar file
```

### Why WHPX rather than a software CPU

- The real CPU gives exact instruction semantics for free: flags,
  SSE/x87 corner cases, `RDTSC`, `CPUID` and so on.
- It runs at native speed. TempleOS spins all cores and redraws at 60 Hz, so
  an interpreter would be very slow.
- WHPX is part of Windows 10 2004+ and Windows 11, including Home, as the
  optional feature "Windows Hypervisor Platform". The exe detects whether
  it's missing and tells the user how to enable it.

We can add a software fallback later (Phase 8), but it isn't on the critical
path.

### Hardware TempleOS needs (the entire device model)

Confirmed against the V5.03 kernel source in Phase 0; the full spec, with
source references, is [`docs/hw-surface.md`](docs/hw-surface.md). The
machine is modelled on QEMU's i440FX + PIIX3 (`-machine pc`), which the
reference traces use too:

| Device | Why TempleOS needs it | Notes |
|---|---|---|
| BIOS (SeaBIOS) | Boot sector, `INT 13h` to load the kernel, `INT 15h E820`, `INT 10h` mode 0x12 | Run the real BIOS; don't fake the calls |
| VGA (planar 640×480×16) | All graphics via 0xA0000 + seq/GC/CRTC/DAC ports | Most performance-critical device |
| 8259 PIC + 8254 PIT | Timer IRQ, PC speaker tone (PIT ch2 + port 0x61) | |
| Local APIC (+ idle I/O APIC) | Multicore (broadcast INIT-SIPI to start APs), tick and wake IPIs | Use WHPX xAPIC emulation mode. The kernel never touches the I/O APIC (one demo does) |
| CMOS/RTC (0x70/0x71) | Date/time | |
| PS/2 controller (0x60/0x64) | Keyboard + mouse | |
| PCI config (0xCF8/0xCFC) | Finding the IDE controller | Tiny PCI bus: host bridge + PIIX3 IDE. TempleOS reaches it only through SeaBIOS's 32-bit PCI BIOS, dropping out of long mode for every call |
| ATA + ATAPI (legacy IDE ports) | CD boot (ATAPI) + install/use HDD | PIO with polling only: no DMA, no bus master |
| HPET (0xFED00000) | Main clock (`tS`, `Busy`, TSC calibration) | Always probed. Only GCAP_ID, GEN_CONF and MAIN_CNT are used |
| Serial 16550 (0x3F8) | Optional: debug log out of the guest | |

Anything else the guest touches gets logged loudly as "unhandled port/MMIO"
during development. Nothing gets silently ignored.

### VGA performance

TempleOS writes planar VGA memory with map-mask plane selection, so every
write to 0xA0000 would cause a VM exit: up to ~19k 64-bit stores per frame at
29.97 frames/s, each split into 8 byte writes. The fast path must not change
anything the guest can see:

- The VGA memory model keeps each plane contiguous and page aligned, so the
  VMM can map one plane read-write at 0xA0000-0xAFFFF with `WHvMapGpaRange`.
  Guest accesses then go to memory with no exits.
- It does so only while that is exact for reads *and* writes
  (`Vga::fast_plane`): planar addressing, exactly one plane write-enabled,
  write mode 0 with no rotate, logic op or set/reset and bit mask 0xFF, and
  read mode 0 with **the same plane selected for reads** (GR04). The mapping
  follows the VGA state before every guest entry. `--exact-vga` turns it off.
- Measured against TempleOS's code, the read-plane condition matters: its
  `ScrnMemory` demo does read-modify-write (`LBts`) on VGA memory with only
  plane 2 enabled and GR04 = 0, so a mapping that ignored GR04 would draw
  differently from real hardware. TempleOS leaves GR04 at 0, so of its four
  plane passes per frame only plane 0's is accelerated; planes 1-3 are
  emulated. Whether that's fast enough is for Phase 3 testing on real
  hardware to show.
- The one guest-visible difference: a read through the mapping doesn't
  reload the latches. While the fast-path conditions hold nothing uses the
  latches; it would only show if the guest read in that state, switched to a
  latch-using write mode, and wrote before reading again.
- The VM thread renders the planes and DAC to RGB 60 times a second, between
  vCPU runs.

## Single-binary packaging

- Build with the Rust MSVC or mingw target, static CRT, so no VC++
  redistributable is needed. The only imports are `WinHvPlatform.dll`
  (loaded lazily so we can show a helpful error), `user32`, `gdi32`/`d3d11`
  and `winmm`/WASAPI.
- The ISO and BIOS blobs go in via `include_bytes!` and are hash-checked at
  compile time.
- **Persistence:** a writable disk can't live inside a running exe. On first
  run we create `TempleOS.hdd` next to the exe, or under
  `%LOCALAPPDATA%\TempleOS\` if that folder is read-only. Without it, the
  user gets the live-CD experience, the same as booting the ISO in a VM.
- Optional: an Authenticode signature so SmartScreen isn't hostile.

## Implementation language

**Rust.** Device emulators handle hostile guest input (bounds on port indexes,
DMA lengths and so on), and memory safety pays off there. The `windows` crate
covers WHPX, Win32 and D3D11. We can cross-compile from Linux CI with
`x86_64-pc-windows-gnu`.

## Phases

### Phase 0 — Groundwork (spec before code)
- [x] Pin SeaBIOS / SeaVGABIOS: `payload/bios.bin`, `payload/vgabios.bin`
  (SeaBIOS 1.16.3, hashes in `payload/SHA256SUMS`).
- [x] Pin the TempleOS V5.03 ISO: size, MD5 and SHA-256 are pinned in
  `payload/TempleOS.ISO.pin` (`tools/fetch-payload.sh` downloads and
  verifies it; the ISO itself is not committed).
- [x] Audit the kernel source for every port, MMIO address, `CPUID` leaf,
  MSR and BIOS call: [`docs/hw-surface.md`](docs/hw-surface.md).
- [x] Reference traces: `tools/qemu-ref/qemu_trace.py` boots the machine in
  QEMU with instruction counting and records port I/O, MMIO, PCI config and
  IRQs plus screenshots; `summarize.py` turns a trace into the surface
  tables. Recorded (in `ref/`, not committed; QEMU 8.2.2 from Ubuntu 24.04
  in Docker): the BIOS alone, and the TempleOS boot through SeaBIOS POST,
  the kernel's switch to long mode, `TimersInit`, the PCI scan and the start
  of the ATA probe. The reference machine runs with `smm=off`, as QEMU does
  on WHPX, which has no SMM.
  - Under `-icount`, each probe of an empty IDE unit takes ~10 minutes of
    wall time (the kernel's timeout is in guest time and TCG is slow), so
    deterministic traces stop in the ATA probe. `--no-icount` runs get
    further (CD found over ATAPI, `DskChg(':')`) but are not repeatable.
  - `tools/qemu-ref/cpuid_ref.sh` captures the `qemu64` CPUID leaves the
    guest sees (`docs/ref/cpuid-qemu64.txt`); the VMM presents exactly these.
  - `tools/qemu-ref/fixtures.py` extracts test fixtures from a trace: PCI
    config space at reset, the VGA EDID, the CD's IDENTIFY data
    (`docs/ref/`).

### Phase 1 — VMM skeleton ✅
- Create the WHPX partition, 1 vCPU and 512 MiB of guest RAM (TempleOS
  minimum; default 1 GiB, as in the reference traces).
- Load SeaBIOS at 0xF0000/0xFFFF0000 and run it until it prints to the debug
  port (0x402). **Milestone: BIOS banner in the log.** Reached ~1 ms after
  power-on (`templeos --until bios-banner`).
- Done in `vmm/`: `whpx.rs` (partition, mappings, registers; register
  arrays must be 16-byte aligned), `memory.rs`, `cpuid.rs` (the captured
  `qemu64` leaves via CPUID exits), `machine.rs` (run loop; I/O and MMIO go
  through the WHPX instruction emulator, as in QEMU's WHPX backend; 8259
  interrupts are injected as ExtINT events on interrupt-window exits, with
  the local APIC emulated by the hypervisor in xAPIC mode; a kicker thread
  cancels the vCPU at the next timer deadline).

### Phase 2 — Boot to kernel ✅
- PIC, PIT, CMOS, PCI host bridge + PIIX IDE, ATAPI serving the embedded ISO.
- **Milestone: the TempleOS boot loader loads `Kernel.BIN.C` and switches to
  long mode** (checked by watching CR0/CR4/EFER on exits). Reached ~0.1 s
  after power-on; the kernel's `TimersInit` follows at ~0.2 s
  (`templeos --until long-mode` / `--until kernel-timers`). Within 3 s the
  kernel has probed IDE, found the CD over ATAPI and copied it in.
- Device models in `devices/` (plain Rust, unit tested, most replayed
  against the reference trace with zero mismatches): PIC + ELCR, PIT +
  port 0x61, RTC/CMOS, PCI (i440FX, PIIX3 ISA/IDE, PIIX4 PM, std VGA),
  PIIX4 PM timer + APM, HPET, fw_cfg (with DMA), IDE/ATAPI, VGA register
  file + Bochs VBE + BAR 2 EDID + ROM BAR, i8042 keyboard/mouse, i8257 DMA.
  `devices/src/pc.rs` is the board (port and MMIO maps, IRQ wiring).
- `--debugcon FILE` and `--trace FILE` (QEMU trace format, readable by
  `tools/qemu-ref/summarize.py`) make runs comparable with the reference.
- Known differences from the reference, all invisible to TempleOS, to
  settle in Phase 6:
  - fw_cfg has no ACPI/SMBIOS tables and no `kvmvapic` option ROM, so
    SeaBIOS builds its own tables and keeps its PM base at 0xB000 (QEMU's
    table loader moves it to 0x600).
  - 0xC0000-0xFFFFF is always RAM (no PAM read-only enforcement).
  - 0xA0000-0xBFFFF is plain RAM until Phase 3's VGA memory model.
  - IDE commands complete instantly, so the guest never sees BSY.

### Phase 3 — Pixels
- [x] VGA memory model (`devices/src/vga.rs`): QEMU's `vga_mem_readb/writeb`,
  i.e. all four write modes, both read modes, latches, rotate/logic ops,
  set/reset, bit mask, chain 4, odd/even and the four memory map modes. VRAM
  is stored planar so a plane can be mapped (see VGA performance above).
- [x] Renderer (`devices/src/vga_render.rs`), following QEMU's
  `vga_draw_text`/`vga_draw_graphic`: text modes (8/9/16-dot cells, two
  fonts, blinking cursor), 16-colour planar, CGA 4-colour, chain-4 256-colour
  and VBE 8/15/16/24/32 bpp, blanking. Frames can be saved as PPM in QEMU
  `screendump` format (`--screenshot`).
- [x] Checked against QEMU: `tools/qemu-ref/vga_ref.py` records a SeaBIOS text
  screen and a mode 12h boot sector that draws like TempleOS (VBE 4F02h,
  map-mask writes, a DAC change). Replaying each trace into the model gives
  zero mismatching reads (38,558 and 116,777 writes), and the rendered frame
  equals QEMU's screendump pixel for pixel
  (`cargo test -p devices replay_vga -- --ignored`; the same test runs on
  `ref/boot/trace.log` by default).
- [x] Board and VMM: 0xA0000-0xBFFFF is now the VGA window (RAM is mapped
  around it); byte-wise emulation traced as QEMU's `vga-lowmem`; the fast-path
  plane mapping; frames at 60 Hz; HLT is a halted state (stays halted with
  IF=0), so the display keeps updating while the guest idles.
- [x] Win32 window (`src/window.rs`): whole-number scaling in a window,
  borderless fullscreen (Right Ctrl+F since Phase 4; 4:3 for VGA modes, square pixels for
  VBE), per-monitor DPI aware. The VM runs on its own thread; a guest reboot
  restarts the machine; closing the window stops it. Started by double-click,
  the console window closes itself. `--headless` keeps the Phase 1-2 CLI.
- **Milestone: the TempleOS desktop renders.** Not yet confirmed: needs a run
  on Windows with the ISO. `--exact-vga` vs default is the first A/B to try
  if anything looks wrong.

### Phase 4 — Input and sound
- [x] Keyboard: the window reads keys with Raw Input (set 1 make codes plus
  E0/E1 flags, so Alt, F10, Pause and Print Screen arrive as real key
  events) and `devices::keymap` turns them into the set 2 bytes a PS/2
  keyboard sends. TempleOS selects set 2 and keeps the controller's
  translation on, so it reads set 1. Every key round-trips through the
  8042 translation table exactly (tested); Print Screen and Pause produce
  QEMU's sequences. Host keyboard repeat is passed through, as QEMU does.
- [x] Mouse: click to capture (hidden, confined to the window), raw relative
  motion, five buttons and the wheel as PS/2 packets. TempleOS's IntelliMouse
  detection finds a wheel mouse and gets 4-byte packets (tested end to end
  by replaying its `KbdInit`/`MsHardRst` port sequence on the board model).
- [x] Host keys (`devices::keymap::HostKeys`, unit tested):
  - Right Ctrl, as in VirtualBox: alone it releases the mouse; Right Ctrl+F
    or +Enter toggles fullscreen. Neither reaches the guest.
  - Ctrl+Alt, as in QEMU, for keyboards without a Right Ctrl: pressed and
    released together with no other key it releases the mouse;
    Ctrl+Alt+Enter toggles fullscreen. Ctrl and Alt still reach the guest,
    so TempleOS's Ctrl+Alt+letter shortcuts (A B C D F G L M N S T V X Z,
    Del) keep working; only the Enter of Ctrl+Alt+Enter, which TempleOS
    doesn't bind, is kept from it.
  - Keys held when the window loses focus are released in the guest.
- [x] Input crosses from the UI thread to the VM thread through
  `devices::input::InputQueue` (mouse motion merged, button changes kept in
  order) and is delivered between vCPU runs.
- [x] PC speaker: the VM reports each tone change (PIT channel 2 in mode 3,
  gated on, speaker data on) with its time; a WASAPI thread plays a square
  wave and applies each change exactly 60 ms after it happened, so note
  lengths are as the guest timed them. Tested with TempleOS's `Snd()` port
  sequence. No audio device: silent, with a note on stderr.
- Not done: the Windows key still opens the Start menu (would need a
  low-level keyboard hook).
- **Milestone: interactive shell, hymns play.** Not yet confirmed: needs a
  run on Windows with the ISO.

### Phase 5 — Multicore, timing and disk
- [x] **Hard disk** (`devices/src/ide.rs`): an ATA disk on the primary
  master with QEMU 8.2's behaviour for every command SeaBIOS and TempleOS
  use (IDENTIFY, READ/WRITE SECTORS and MULTIPLE incl. EXT, READ NATIVE MAX
  (EXT), SET MULTIPLE MODE, VERIFY, SEEK, INITIALIZE DEVICE PARAMETERS,
  FLUSH CACHE; SET MAX aborts as in QEMU), QEMU's geometry guess and the
  disk CMOS bytes. Checked against QEMU with `tools/qemu-ref/disk_ref.py`,
  a boot disk that drives the controller exactly like TempleOS's
  `DskATA.HC` (one sector per DRQ in WRITE MULTIPLE EXT, HOB reads of READ
  NATIVE MAX EXT...): 0 mismatches in 35,836 register reads, and the disk
  image the model writes is byte-identical to QEMU's.
- [x] Disk image: `TempleOS.hdd` (2 GiB, created on first run) next to the
  exe, or in `%LOCALAPPDATA%\TempleOS` when that folder is read-only;
  `--hdd PATH`, `--no-hdd`. Writes go straight to the file; FLUSH CACHE
  syncs it. With a disk the boot order is QEMU's default (disk first, CD
  last): a blank disk falls through to the CD, an installed one boots.
- [x] **Multicore** (`vmm/src/machine.rs`): one thread per vCPU (default:
  the host's cores up to 8 with a window, 1 headless; `--cpus N`). The
  board is shared behind a mutex. INIT and SIPI trap to the VMM and are
  re-issued with `WHvRequestInterrupt`, as QEMU's WHPX backend does; APs
  start in wait-for-SIPI, and an AP's thread is parked before an INIT
  reaches it so no stale exit handling touches the fresh state. Other IPIs
  are delivered by the hypervisor's local APICs. A halted vCPU wakes on a
  pending interrupt in its local APIC's IRR (polled every 250 us) or, for
  the BSP, the PIC; with IF=0 it stays halted until INIT. CPUID reports
  QEMU's `-smp N` topology (checked against captures for 2, 4 and 8 CPUs),
  and fw_cfg and CMOS 0x5F the CPU count, so SeaBIOS and TempleOS find
  every core the way they do in QEMU.
- [x] **Timing**: PIT, HPET and RTC are computed from the one host
  monotonic clock; the guest TSC is the host TSC (kept in step across
  vCPUs by the hypervisor). TempleOS calibrates the TSC against the HPET
  at boot (`TimeCal`), so `Sleep`, `tS` and the TSC agree by construction.
- [x] The timer-kicker thread now stops with its machine (before, every
  guest reboot leaked a partition).
- Not modelled: the I/O APIC (the kernel never touches it; only the
  `PCIInterrupts` demo reads it); INIT/SIPI in logical destination mode
  without a shorthand (passed to the hypervisor as is; SeaBIOS and TempleOS
  always use shorthands).
- **Milestone: installs to C:, reboots from HDD, all cores show in the task
  bar.** Not yet confirmed: needs a run on Windows with the ISO. The new
  `--until ap-started` milestone (first SIPI) is the quick multicore check.

### Phase 6 — Proving "perfect"
Done and passing here (no ISO or Windows needed):
- [x] **Whole-board replay** (`devices/tests/replay_board.rs`): a complete
  QEMU reference trace replayed into the whole board through its own
  routing, every read that doesn't depend on time compared. On the three
  ISO-free references (SeaBIOS text screen, mode 12h boot sector, ATA probe
  disk), each including SeaBIOS's full POST: 117,734 / 143,318 / 101,080
  accesses, 0 mismatches. It found one real bug (the CMOS boot order
  without a CD). Time-dependent reads (PIT, RTC time, HPET/PM timer
  counters, PIC IRR/ISR, fw_cfg after DMA, IDE BSY polls) are executed but
  not compared: the trace has no timestamps.
- [x] **Per-device replays** (Phases 2-5): IDE (incl. the disk image),
  PCI, VGA (reads + final frame vs QEMU's screendump, pixel exact).
- [x] **Keyboard vs QEMU** (`devices/tests/replay_input.rs`,
  `tools/qemu-ref/input_ref.py`): every printable ASCII character and
  every other key, sent to QEMU with QMP and through our script path: the
  436 bytes SeaBIOS read from port 0x60 are identical. (It caught one
  naming slip: QEMU's context-menu key is `compose`.)
- [x] **Fuzzing** (`devices/tests/fuzz_board.rs`): random and structured
  guest behaviour against the whole board; no panics, no operation may
  stall the host. It found two overflow bugs (text cursor position; CHS
  sector wrap), both fixed. 30M+ operations clean since.
- [x] **Payload integrity**: build.rs pins, and the exe re-checks the
  SHA-256 of its embedded BIOS, VGA BIOS and ISO at every start.

Ready, needs a Windows machine with the ISO. `tools/phase6-windows.ps1`
runs the first two in one go and writes `phase6-results/summary.txt`:
- [ ] **Scripted differential runs**: `templeos.exe --script S --shots A`
  and `tools/qemu-ref/qemu_trace.py --script S --wait-scale 10 --out B`
  run the same input script (`devices/src/script.rs`; type/key/mouse/
  wait/screenshot) on both machines; `tools/compare_shots.py A B --mask
  0:8` compares the screenshots pixel for pixel (the mask hides TempleOS's
  clock line). Starter scripts: `tests/scripts/cd_smoke.script` and
  `tests/scripts/demos.script` (ScrnMemory, the VGA fast-path case; Hanoi;
  Palette).
- [ ] **Whole-board replay of the TempleOS boot**:
  `cargo test -p devices --test replay_board -- --ignored` against
  `ref/boot/trace.log` (the kernel's own device traffic).
- [ ] **In-guest tests**: run the demos and the kernel self-compile
  (`BootHDIns`) from a script, and check they complete.
- Not planned: guest RAM hashes at an idle point. WHPX runs guest code on
  the real CPU at real speed, so two runs never reach the same instruction
  at the same timer tick; screenshots after the guest settles are the
  comparable state.

### Phase 7 — Packaging and release
- A static single exe, a reproducible build (pinned toolchain + blobs →
  identical exe hash), a GitHub Actions workflow producing `TempleOS.exe`
  plus SHA-256.
- Friendly first-run error if WHPX is off, including a one-line PowerShell
  command to enable it.

Done:
- [x] **Static exe**: static CRT (`.cargo/config.toml`), no Visual C++
  redistributable. `tools/pe_imports.py --check` fails the build unless every
  load-time import is part of Windows.
- [x] **Starts without WHPX**: the WHPX DLLs are delay-loaded (build.rs,
  MSVC), and `whpx::check_available` probes for them before any WHv call.
  Two messages: feature not installed (the PowerShell command), or
  installed but the hypervisor isn't running (restart, VT-x/AMD-V in the
  firmware, `bcdedit /set hypervisorlaunchtype auto`).
- [x] **Reproducible**: toolchain pinned (`rust-toolchain.toml`),
  `Cargo.lock` with `--locked`, build paths remapped out of the binary,
  deterministic link (`/Brepro`, `/PDBALTPATH:%_PDB%`).
  `tools/build-release.ps1` makes `dist/TempleOS.exe` + `.sha256` + `.pdb`.
- [x] **CI** (`.github/workflows/build.yml`): device tests and a 2M-op fuzz
  run on Linux; on Windows fetch and verify the ISO (cached), run all tests,
  build the release, build it again from another folder and require the same
  hash, upload the exe. A `v*` tag publishes a GitHub release.
  Hosted runners can't boot the VM (no nested virtualization); booting stays
  with `tools/phase6-windows.ps1` on a real PC.

Not done:
- [ ] First green CI run (the MSVC link options are untested until then).
- [ ] Optional: an Authenticode signature (needs a certificate), an icon
  and version resource.

### Phase 8 (optional) — No-hypervisor fallback
- For machines without virtualization, embed a software x86-64 CPU. To keep
  it "byte-instruction perfect" this must be a proven core (e.g. the Bochs
  CPU, via FFI) and not a new one, and it must pass the same Phase 6 suite.
  It'll be slow but correct.

## Risks

| Risk | Mitigation |
|---|---|
| WHPX not enabled / Hyper-V conflicts | Clear detection + instructions; Phase 8 fallback |
| VGA exit storm | Plane remap fast path (above) |
| Timing drift makes the guest run too fast or slow | Derive PIT/HPET from host QPC; keep TSC passthrough |
| Multicore races in the device model | Single device lock at first; profile later |
| Disk corruption on host crash | Flush on guest ATA FLUSH CACHE; the image is plain raw |

## Licensing

- TempleOS: public domain, so it's fine to embed.
- SeaBIOS / SeaVGABIOS: LGPLv3. Ship the pinned source tarball URLs + build
  script in the repo, and let the blobs be replaced (they're loadable from
  a file override flag).

## Repo layout (proposed)

```
/vmm        Rust crate: partition, vCPU loop, memory
/devices    Rust crate: pic, pit, rtc, ps2, vga, ide, pci, apic glue, hpet
/frontend   Rust crate: window, input, audio
/payload    BIOS blobs + SHA256SUMS, ISO pin (ISO itself fetched, not committed)
/tools      QEMU trace plugin, differential test harness
/docs       hw-surface.md, test reports
```
