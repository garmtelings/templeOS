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

To be confirmed against the V5.03 kernel source (`/Kernel/*.HC`) in Phase 0:

| Device | Why TempleOS needs it | Notes |
|---|---|---|
| BIOS (SeaBIOS) | Boot sector, `INT 13h` to load the kernel, `INT 15h E820`, `INT 10h` mode 0x12 | Run the real BIOS; don't fake the calls |
| VGA (planar 640×480×16) | All graphics via 0xA0000 + seq/GC/CRTC/DAC ports | Most performance-critical device |
| 8259 PIC + 8254 PIT | Timer IRQ, PC speaker tone (PIT ch2 + port 0x61) | |
| Local APIC + I/O APIC | Multicore (INIT-SIPI to start APs), IPIs | Use WHPX xAPIC emulation mode |
| CMOS/RTC (0x70/0x71) | Date/time | |
| PS/2 controller (0x60/0x64) | Keyboard + mouse | |
| PCI config (0xCF8/0xCFC) | Finding the IDE controller | Tiny PCI bus: host bridge + PIIX IDE |
| ATA + ATAPI (legacy IDE ports) | CD boot (ATAPI) + install/use HDD (ATA PIO) | |
| HPET (0xFED00000) | Timing, if the kernel uses it | Verify in Phase 0 |
| Serial 16550 (0x3F8) | Optional: debug log out of the guest | |

Anything else the guest touches gets logged loudly as "unhandled port/MMIO"
during development. Nothing gets silently ignored.

### VGA performance

TempleOS writes planar VGA memory with map-mask plane selection, so every
write to 0xA0000 would cause a VM exit (millions per second). The fix below
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
- Pin the artifacts: TempleOS V5.03 ISO (SHA-256), SeaBIOS and SeaVGABIOS
  versions.
- Audit the TempleOS kernel source for every `InU8/OutU8/…` port, MMIO
  address, `CPUID` leaf and MSR it touches, and produce `docs/hw-surface.md`.
  This list is the device model's spec.
- Record reference traces: boot the ISO in QEMU (`-d int,cpu_reset`, a
  port-I/O trace via a small QEMU plugin) to capture the exact order of
  device interactions.

### Phase 1 — VMM skeleton
- Create the WHPX partition, 1 vCPU and 512 MiB of guest RAM (TempleOS
  minimum; default 2 GiB).
- Load SeaBIOS at 0xF0000/0xFFFF0000 and run it until it prints to the debug
  port (0x402). **Milestone: BIOS banner in the log.**

### Phase 2 — Boot to kernel
- PIC, PIT, CMOS, PCI host bridge + PIIX IDE, ATAPI serving the embedded ISO.
- **Milestone: the TempleOS boot loader loads `Kernel.BIN.C` and switches to
  long mode** (checked by watching CR0/CR4/EFER on exits).

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
/payload    pinned ISO + BIOS blobs (Git LFS) + SHA256SUMS
/tools      QEMU trace plugin, differential test harness
/docs       hw-surface.md, test reports
```
