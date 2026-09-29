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
29.97 frames/s, each split into 8 byte writes. The fix below
doesn't change any guest-visible behavior:

- Watch the sequencer map-mask and graphics-controller mode registers
  (port exits, which are cheap and rare).
- When the state is "write mode 0, a single plane enabled, no rotate or
  logic op", which is how TempleOS writes a frame, **remap the 0xA0000 guest
  window straight onto that plane's backing pages** with
  `WHvMapGpaRange`. Guest writes then go to plain RAM with no exits.
- In any other VGA state, fall back to exact MMIO emulation of every access.
- The frontend reads the 4 plane buffers + DAC palette, converts to RGBA and
  presents at 60 Hz.

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
- VGA: full register file, planar memory, DAC, and the fast-path remap above.
- Win32 window with integer-scaled 640×480 and aspect-correct fullscreen.
- **Milestone: the TempleOS desktop renders.**

### Phase 4 — Input and sound
- PS/2 keyboard (scan code set 1, which is what TempleOS reads) and PS/2
  mouse. Raw-input capture, with a host-key escape (Right Ctrl) like other
  VMs.
- PC speaker: turn PIT ch2 frequency + port 0x61 gate into a square wave on
  WASAPI.
- **Milestone: interactive shell, hymns play.**

### Phase 5 — Multicore, timing and disk
- More vCPUs (TempleOS uses all cores; default = host cores, capped at 8),
  LAPIC IPIs, INIT/SIPI handling, I/O APIC routing.
- Make `RDTSC`/HPET/PIT consistent with each other so `Sleep()` and the
  frame rate are correct.
- ATA PIO HDD backed by the sidecar image, so the in-guest `Install` works.
- **Milestone: installs to C:, reboots from HDD, all cores show in the task
  bar.**

### Phase 6 — Proving "perfect"
- **Image integrity:** startup self-check of the embedded ISO hash. CI fails
  if the payload ever differs from the pinned hash.
- **Differential device testing:** a record/replay harness feeds identical
  scripted input (keystrokes, timer ticks) to QEMU and to our VMM and
  compares:
  - the sequence of port I/O and MMIO accesses + values returned,
  - framebuffer hashes at checkpoints,
  - guest RAM hashes after boot reaches a deterministic idle point
    (single-core, fixed TSC).
- **In-guest tests:** run TempleOS's own demos and self-tests
  (`/Demo/*`, compiling the kernel from source in-guest with `BootHDIns`)
  and check that they complete.
- Fuzz the device models (port/MMIO sequences) so malformed guest behavior
  can't crash the host.

### Phase 7 — Packaging and release
- A static single exe, a reproducible build (pinned toolchain + blobs →
  identical exe hash), a GitHub Actions workflow producing `TempleOS.exe`
  plus SHA-256.
- Friendly first-run error if WHPX is off, including a one-line PowerShell
  command to enable it.

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
