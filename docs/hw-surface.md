# TempleOS V5.03 hardware surface

This is the device-model spec for the VMM. It lists everything the TempleOS
kernel and Adam layer do to hardware: CPU features, firmware calls, I/O
ports and MMIO. If something is not in this document, TempleOS does not touch
it and the VMM does not need it. The one exception is SeaBIOS's own POST,
which the VMM has to satisfy too; the QEMU reference traces capture that part.

Source audited: [cia-foundation/TempleOS](https://github.com/cia-foundation/TempleOS)
tag `final` (commit `c26482b`), the V5.03 tree. File references below are
paths in that repository. Demos and optional utilities that poke hardware on
their own (for example `Demo/Lectures/PCIInterrupts.HC`, which touches the
IOAPIC) are called out separately at the end.

How to read the "Fidelity" notes: they describe behavior the kernel depends
on that a naive device model would get wrong.

## 1. Boot flow and firmware (BIOS) services

TempleOS boots through the real BIOS, and keeps calling into the BIOS after
boot. The VMM runs unmodified SeaBIOS (`payload/bios.bin`) and SeaVGABIOS
(`payload/vgabios.bin`), so every call below is served by real firmware code.
The list is the requirement on that firmware.

| When | Call | Purpose | Source |
|---|---|---|---|
| CD boot sector (real mode) | `INT 10h AH=0Fh` | get video page | `Adam/Opt/Boot/BootDVD.HC` |
| CD boot sector | `INT 10h AH=0Eh` | teletype progress output | same |
| CD boot sector | `CPUID 0x80000000`, `0x80000001` (EDX bit 29 = LM) | refuse to boot on non-64-bit CPUs | same |
| CD boot sector | `INT 13h AH=42h` (EDD extended read) | load `Kernel.BIN.C` from the CD, 2048-byte blocks | same |
| HD boot (after install) | `INT 13h AH=42h` | load the kernel from the hard disk | `Adam/Opt/Boot/BootHD.HC`, `BootMHD*.HC` |
| `KStart16` | `INT 10h AX=4F02h BX=0012h` | **VBE set mode with legacy mode 12h** (640×480×16 planar). Success is `AX=004Fh`; otherwise TempleOS falls back to text mode | `Kernel/KStart16.HC` |
| `KStart16` | `INT 15h AX=E801h` | memory size | same |
| `KStart16` | `INT 15h EAX=E820h` (up to 47 entries) | memory map | same |
| `KStart16` | `INT 1Ah AX=B101h` | PCI BIOS present? (last bus number) | same |
| `KStart32` | scan 0xE0000–0xFFFFF for the `_32_` **BIOS32 Service Directory**, then call it with `$PCI` | locate the 32-bit PCI BIOS entry point | `Kernel/PCIBIOS.HC` |
| Any time (runtime!) | 32-bit far call into the PCI BIOS: `B103h` (find class code), `B108h`/`B109h`/`B10Ah` (config read 8/16/32), `B10Bh`/`B10Ch`/`B10Dh` (config write 8/16/32) | **all** PCI config access | `Kernel/PCIBIOS.HC` |

Consequences for the VMM:

- **PCI config access leaves long mode.** Every `PCIRead*`/`PCIWrite*`/
  `PCIClassFind` runs `_FAR_CALL32`. It IRETs to 32-bit code, turns off
  paging (CR0.PG=0), clears `IA32_EFER`, far-calls SeaBIOS's PCI BIOS (which
  in turn uses ports 0xCF8/0xCFC), and then re-enters long mode. The vCPU
  switches modes many times at runtime. With WHPX the hardware handles this;
  the device model only sees the resulting 0xCF8/0xCFC accesses.
- SeaBIOS must be built with `CONFIG_PCIBIOS` (BIOS32 directory + `$PCI`
  service). The pinned blob has this: at runtime the directory sits at
  0xF60A0 with entry point 0xFD27B (checked in QEMU with `xp`).
- SeaVGABIOS must accept VBE function 4F02h with a legacy mode number
  (0x12). If it didn't, TempleOS would silently boot in text mode.
  **Verified:** with the pinned blobs, a test boot sector making this exact
  call gets `AX=004Fh`, and SeaVGABIOS logs `VBE mode set: 12`.
- The E820 map decides how much RAM TempleOS uses, and it requires
  ≥ 512 MiB (`MEM_MIN_MEG`, `Kernel/KernelA.HH`).
- **Reboot** (`Kernel/KMain.HC: Reboot`): writes 0 to the BIOS warm-boot
  flag at 0x472, writes CMOS register 0x0F (shutdown status) = 0 with NMI
  disabled (index 0x8F), then sets bit 0 of port 0x92 (fast reset). The VMM
  must turn that into a full platform reset back into SeaBIOS.

## 2. CPU

| Feature | Use | Source |
|---|---|---|
| Long mode | Required. Entered from 32-bit PM with CR4 \|= 0xB0 (PSE, PAE, PGE), EFER.LME, CR0.PG | `Kernel/KStart64.HC` |
| Paging | **2 MiB pages only.** The 1 GiB-page path is commented out, even though CPUID 0x80000001 is queried. Physical RAM is identity mapped, plus an *uncached alias* of the low 4 GiB above RAM (PTE flags 0x197: P, RW, US, PWT, PCD, PS, G) | `Kernel/Mem/PageTables.HC`, `Kernel/Mem/MemPhysical.HC` |
| `CPUID 1` | EBX[15:8] × 8 → cache-line width; EDX bit 9 (APIC) gates multicore start | `Kernel/KMain.HC`, `Kernel/MultiProc.HC` |
| `CPUID 0x80000000/1` | boot sector LM check; 1 GiB-page probe (result unused) | see above |
| MSRs written | `IA32_EFER` (0xC0000080, only LME), `IA32_FS_BASE`, `IA32_GS_BASE` (per task switch). **No `RDMSR` anywhere.** `IA32_APIC_BASE` write is commented out | `Kernel/KUtils.HC`, `Kernel/KStart64.HC` |
| `RDTSC` | The time base. Calibrated against the HPET (or PIT) by `TimeCal`; keyboard/mouse timeouts, cursor blink and `rand` seeds use it | `Kernel/KMisc.HC` and others |
| x87 + `FXSAVE`/`FXRSTOR` | Task switch saves the FPU state. CR4.OSFXSR is **not** set, so XMM state is not part of the image | `Kernel/Sched.HC` |
| `HLT` | Idle loop with `STI; HLT` | `Kernel/Sched.HC` |
| `WBINVD`, `INVLPG`, `LGDT`, `LIDT`, `PAUSE`, `LOCK`-prefixed ops | Standard | various |
| Ring 3 | Only in a demo (`Demo/Lectures/Ring3.HC`); a GDT entry exists for it | `Kernel/KStart16.HC` |

Guest CPUID should present a plain, stable x86-64 CPU: long mode, APIC,
TSC, FXSR, clflush line size = 8 (64 bytes). Don't pass through the host's
brand string and exotic leaves: they differ between machines and would leak
into `CPURep`. The reference traces use QEMU's `qemu64` model, and the VMM
should report the same leaves so traces match.

## 3. Interrupt controllers

### 8259A PIC pair (ports 0x20/0x21, 0xA0/0xA1) — *the IRQ path for devices*

`Kernel/KInts.HC: IntInit1`:

```
0x20 <- 0x11   0xA0 <- 0x11     ICW1: edge, cascade, ICW4 needed
0x21 <- 0x20   0xA1 <- 0x28     ICW2: vectors 0x20-0x27 / 0x28-0x2F
0x21 <- 0x04   0xA1 <- 0x02     ICW3
0x21 <- 0x0D   0xA1 <- 0x09     ICW4: 8086 mode, *buffered* master / slave
0x21 <- 0xFA   0xA1 <- 0xFF     mask all but IRQ0 (timer) and IRQ2 (cascade)
```

Later: IRQ1 (keyboard, vector 0x21) and IRQ12 (mouse, vector 0x2C) are
unmasked by read-modify-writing the mask registers. EOIs are non-specific
(`0x20 <- 0x20`, plus `0xA0 <- 0x20` for the mouse).

**Fidelity:** `SysTimerRead` (`Kernel/KMisc.HC`) latches PIT channel 0 and,
if the count equals the reload value, reads **port 0x20 to get the IRR**
(OCW3 default read register). IRR bit 0 tells it whether a timer overflow is
still pending. So the IRR state and the default read-register selection must
be exact, or `tS`/`Sleep` jump backwards by one tick.

`PortNop` reads port 0x21 purely as a ~1 µs delay.

### Local APIC (MMIO 0xFEE00000) — *multicore and inter-core IPIs*

All accesses go through the uncached alias, but the guest-physical address is
still 0xFEE00000. Registers touched (`Kernel/MultiProc.HC`, `Kernel/KInts.HC`):

| Register | Use |
|---|---|
| SVR (0x0F0) | set bit 8 (APIC enable) on each core |
| APIC ID (0x020) | read to build `mp_apic_ids[]` |
| LDR (0x0D0), DFR (0x0E0) | flat logical mode, LDR = id<<24 |
| LVT Error (0x370) | low byte set to `MPN_VECT` (0x97) |
| ICR high/low (0x310/0x300) | IPIs: INIT all-but-self (`0xC4500`), SIPI all-but-self vector 0x97 (`0xC4600+0x97`, sent twice), fixed IPI to one core (`0x4000+vec`, physical destination), fixed IPI to all-but-self (`0xC4800+vec`), NMI all-but-self (`0xC4400`, used by `MPHalt`). Polls the delivery-status bit 12 before each send |
| EOI (0x0B0) | written in `IRQ_TIMER` and other handlers on every core |

The LAPIC timer is **not used**. Application processors get their timer
tick as a fixed IPI (vector 0x20) sent by core 0 from its PIT interrupt.

AP bring-up: core 0 copies a real-mode trampoline to 0x97000 (`MP_VECT_ADDR`),
sends INIT, waits 10 ms (`Busy`), sends SIPI twice, waits 100 ms, and counts
the APs that incremented `SYS_MP_CNT_INITIAL`. There is no MP table or ACPI
MADT parsing: **the number of cores is however many answer the broadcast
SIPI**, capped at 128 (`MP_PROCESSORS_NUM`).

Core 0 gets PIC interrupts through LINT0 in ExtINT (virtual-wire) mode, as
SeaBIOS leaves it. TempleOS never reprograms LINT0/LINT1.

### I/O APIC (MMIO 0xFEC00000)

Not used by the kernel. Only `Demo/Lectures/PCIInterrupts.HC` reads it. Keep
it present with QEMU-identical reset state so that demo matches.

## 4. Timers

### 8254 PIT (ports 0x40–0x43, 0x61)

| Access | Meaning | Source |
|---|---|---|
| `0x43 <- 0x34`, `0x40 <- 0xA8`, `0x40 <- 0x04` | channel 0, mode 2 (rate generator), count 0x04A8 = 1192 → 1000.15 Hz (`JIFFY_FREQ`) | `TimersInit` |
| `0x43 <- 0x00`, then two reads of 0x40 | latch + read channel 0 count (low, high) | `SysTimerRead` |
| `0x43 <- 0xB6`, `0x42 <- lo`, `0x42 <- hi`, `0x61 \|= 3` | channel 2 square wave to the PC speaker | `Snd`, `Kernel/KMisc.HC` |
| `0x61 &= ~3` | speaker off | `KMain`, `Snd(0)` |

Channel 0 drives IRQ0 → vector 0x20 on core 0, which increments `jiffies`
and forwards a tick IPI to every other core.

### HPET (MMIO 0xFED00000) — *primary clock when present*

`TimersInit` (`Kernel/KMain.HC`):

1. If PCI 00:1F.0 vendor = 0x8086 (an Intel ICH LPC bridge), it reads the
   RCBA from config register 0xF0 and enables the HPET in chipset register
   RCBA+0x3404. On i440FX there is no device at 00:1F.0, so this is skipped.
2. **Unconditionally** reads the 64-bit `GCAP_ID` at 0xFED00000. If the
   period (upper 32 bits, femtoseconds) is between 100000 and 10⁹, the HPET
   is used: it sets `GEN_CONF` bit 0 (enable) and reads the 64-bit
   `MAIN_CNT` (0xFED000F0) from then on for `tS`, `Busy` and TSC calibration.
3. Otherwise the kernel falls back to the PIT for all timekeeping.

Only these three registers are accessed, all as 64-bit. Timer comparators and
HPET interrupts are never used.

**Decision:** provide an HPET with QEMU's exact capability value
(`0x009896808086A201`: 10 ns period = 100 MHz, vendor 0x8086, 3
comparators, legacy-route capable, 64-bit counter). With it, `cnts.HPET_freq` and the TSC calibration
match the reference traces. Note: the "TempleOS_vanilla_no_HPET" respin
exists because some *real* machines' HPETs hang TempleOS. That's irrelevant
here, but it shows how much the kernel relies on this path.

### CMOS RTC (ports 0x70/0x71)

`Kernel/KDate.HC: NowDateTimeStruct`: waits for status A bit 7 (UIP) to
clear, reads 0x00 (sec), 0x02 (min), 0x04 (hour), 0x06 (weekday), 0x07
(day), 0x08 (month), 0x09 (year), loops until UIP is still clear, then reads
status B bit 2 to choose BCD or binary. No century register, no alarms, no
RTC interrupts. Reboot writes register 0x0F (see §1).

**Fidelity:** the UIP window has to be emulated (set for 244 µs before each
update), because the kernel's loop depends on it.

## 5. Graphics — VGA

Mode: 640×480, 16 colors, planar (mode 12h), set by SeaVGABIOS through the
VBE call in §1. After that the kernel and Adam program the VGA directly:

| Port | Use | Source |
|---|---|---|
| 0x3C4/0x3C5 | Sequencer index 2 (Map Mask): 0x0F to clear the screen, then 1, 2, 4, 8 for each plane on every screen update | `Kernel/KMain.HC`, `Adam/Gr/GrScrn.HC` |
| 0x3DA (read) | reset the attribute-controller flip-flop | `Adam/Gr/GrPalette.HC` |
| 0x3C0 | attribute palette registers 0–15 set to identity, then index 0x20 (PAS) to re-enable video | same |
| 0x3C6 | DAC pixel mask = 0xFF | same |
| 0x3C8 / 0x3C7 | DAC write / read index | same |
| 0x3C9 | DAC data, 6-bit R, G, B (read and write) | same |
| Memory 0xA0000–0xA95FF | the frame buffer: 38400 bytes per plane | `Adam/Gr/GrScrn.HC` |

Writes to the frame buffer:

- They happen in `GrUpdateVGAGraphics`, **29.97 times a second**
  (`WINMGR_FPS` = 30000/1001, driven by the window manager), as **64-bit stores** of only the qwords that changed since
  the previous frame (`GrUpdateLine64`). Each frame is four passes, one per
  plane, with Map Mask = 1, 2, 4, 8 and the graphics controller left in
  write mode 0 with no rotate or logic op and bit mask 0xFF (SeaVGABIOS
  defaults for mode 12h).
- A full frame is at most 4 × 4800 qword stores. That's why the VMM maps
  a plane's backing memory straight into the guest while that is exact
  (`Vga::fast_plane`: planar addressing, one plane write-enabled, write mode
  0 with no rotate/logic op/set-reset on that plane, bit mask 0xFF, read mode
  0 **reading the same plane**). Every access in any other VGA state is
  emulated exactly, and a 64-bit store is split into eight byte-wide VGA
  writes in address order, as QEMU's byte-wide `vga-lowmem` region does.
- **Reads matter.** The kernel never reads VGA memory (`MiniGrLib.HC`: "0xA0000
  alias memory can't be read"), but `Demo/Lectures/ScrnMemory.HC` plots with
  `LBts` (a locked read-modify-write) on VGA memory with Map Mask = RED
  (plane 2) and GR04 = 0: the read returns plane 0, the write goes to plane 2.
  So TempleOS's frame updates, which leave GR04 at 0, only qualify for the
  mapping on the plane 0 pass.
- A mapped plane doesn't reload the latches on reads. That can't matter while
  the mapping conditions hold (no write mode uses the latches then); it
  would only be visible if the guest read in that state, switched to a
  latch-using mode, and wrote before any other read.
- Text mode (0xB8000) is only used if the VBE call failed, or with the
  `CFG_TEXT_MODE` kernel option.

## 6. Input — i8042 PS/2 controller (ports 0x60/0x64)

Keyboard (`Kernel/SerialDev/Keyboard.HC`):

- Controller commands: 0xA7 (disable mouse port), 0xAE (enable keyboard),
  0xA8 (enable mouse port), 0xAD (disable keyboard, during mouse probing),
  0x20/0x60 (read/write command byte: sets bit 1 = IRQ12 enable and clears
  bit 5 = mouse clock disable), 0xD4 (next byte to mouse).
- Keyboard commands: 0xF0 0x02 (**select scan code set 2**; the controller's
  translation, which stays on, delivers set 1 to the OS), 0xF3 (typematic),
  0xED (LEDs).
- Reads the status register (0x64) for bit 0 (output full) and bit 1 (input
  full) in polled loops, and reads data (0x60) in the IRQ1 handler.

Mouse (`Kernel/SerialDev/Mouse.HC`):

- Before probing, if the BIOS data area's EBDA segment (0x40E) is 0x9FC0,
  it writes 1 to EBDA offset 0x30 (0x9FC30), a hack for one of Terry's
  machines. With the pinned SeaBIOS the EBDA segment **is** 0x9FC0 (checked
  with a test boot sector), so this write happens on every boot. It lands in
  guest RAM, inside SeaBIOS's EBDA, so the EBDA layout must match the
  reference.
- Reset 0xFF, then the IntelliMouse knock `F3 200, F3 100, F3 80` and
  `F2` (read ID: 3 = wheel, 4 = 5 buttons), then `F3 10` and `F2` again,
  then `E8 03` (resolution), `E6` (scaling 1:1), `F3 100`
  (sample rate), `F4` (enable streaming).
- Packets arrive on IRQ12 (vector 0x2C), 3 or 4 bytes depending on the ID.
  The status register's bit 5 (AUX data) must be correct, because the
  handler uses it to separate mouse bytes from keyboard bytes.

## 7. Storage — ATA / ATAPI (PIO only)

Discovery (`Kernel/BlkDev/DskATAId.HC`):

- `PCIClassFind(0x0101xx, n)` over all 256 prog-IFs, via the PCI BIOS. For
  each IDE controller, BAR0–3 are used only if they are I/O BARs (bit 0
  set). Otherwise it falls back to legacy 0x1F0/0x3F6 (primary) and
  0x170/0x376 (secondary). The i440FX's PIIX3 IDE is in compatibility mode
  with BARs 0–3 reading 0, so the legacy ports are used.
- At boot, `BootDVDProbeAll` looks on every channel and unit for the ATAPI
  device whose block `sys_boot_blk` holds a kernel with the same compile time
  as the running one. **The CD must be readable through ATAPI, not just
  INT 13h.**

Registers: data (base0+0, 16- and 32-bit `REP INS/OUTS`), feature/error
(+1), sector count (+2), LBA low/mid/high (+3/+4/+5), drive select (+6),
status/command (+7), and device control / alternate status (base1+2). The kernel writes 0x08 there
(bit 3, the obsolete always-one bit; nIEN is **not** set) and 0x80 (HOB) to
read the high LBA bytes. **No DMA, no bus-master registers.** All transfers
are PIO with polling. The drive still raises IRQ14/15, but they stay masked
at the PIC, so the IRR bits matter only for fidelity of PIC reads.

ATA commands: `00h` NOP, `08h` DEVICE RESET, `ECh` IDENTIFY, `27h` READ NATIVE
MAX EXT (the code has a comment "Kludge to make QEMU work"), `F8h` READ NATIVE
MAX, `37h`/`F9h` SET MAX (EXT), `29h`/`C4h` READ MULTIPLE (EXT), `39h`/`C5h`
WRITE MULTIPLE (EXT), `A0h` PACKET.

ATAPI packets: `1Bh` START STOP UNIT, `25h` READ CAPACITY, `2Bh` SEEK, `A8h`
READ(12), `BBh` SET CD SPEED, `52h` READ TRACK INFORMATION, and for DVD
burning `04h` FORMAT UNIT, `AAh` WRITE(12), `35h` SYNCHRONIZE CACHE, `5Bh`
CLOSE TRACK. The byte count limit is taken from LBA mid/high (+4/+5).

**Fidelity:** the kernel polls BSY and DRQ with timeouts measured in `tS`
(HPET time). Status transitions have to follow the ATA state machine in the
right order, but they don't need realistic latency.

## 8. Misc ports

| Port | Use |
|---|---|
| 0x92 | A20 enable (bit 1) in `KStart16`; fast reset (bit 0) in `Reboot` |
| 0x61 | PC speaker gate/data (bits 0–1), read-modify-write |
| 0xCF8/0xCFC | only through SeaBIOS's PCI BIOS (§1) |
| 0x402 | SeaBIOS debug console (VMM debug builds only; not touched by TempleOS) |

No serial ports, no parallel ports, no floppy controller, no ISA DMA, no
ACPI, and no power-off: TempleOS has no shutdown; the user closes the window.

## 9. Memory map the VMM must present

| Range | Contents |
|---|---|
| 0x00000–0x9FFFF | RAM (conventional). The CD boot loader relocates itself just below `BOOT_RAM_LIMIT` = 0x97000 and loads the kernel at 0x7C00 (`BOOT_RAM_BASE`); AP trampoline at 0x97000; EBDA at the top (0x9FC00 with SeaBIOS) |
| 0xA0000–0xBFFFF | VGA window (§5) |
| 0xC0000–0xDFFFF | option ROMs (SeaVGABIOS at 0xC0000) |
| 0xE0000–0xFFFFF | BIOS, including the BIOS32 directory scanned in §1 |
| 0x100000– | RAM. The kernel's fixed area is at `SYS_FIXED_AREA` = 0x100000 |
| 0xFEC00000 | I/O APIC |
| 0xFED00000 | HPET |
| 0xFEE00000 | Local APIC |
| 0xFFFC0000–0xFFFFFFFF | BIOS ROM (256 KiB image) |

RAM size: the plan defaults to 1 GiB (the reference traces use the same), and
the minimum is 512 MiB.

## 9a. What SeaBIOS's POST needs on top

Measured from the reference traces (`ref/bios-only`, `ref/boot`). TempleOS
never touches these, but the firmware does, so the VMM models them:

| Device | Accesses | Model |
|---|---|---|
| QEMU fw_cfg (0x510 selector, 0x511 data, 0x514 DMA) | signature, file directory, `etc/e820`, CPU count, ACPI/SMBIOS tables | `devices/src/fwcfg.rs`; the VMM provides the e820 map and CPU counts. ACPI and SMBIOS tables are not provided yet, so SeaBIOS builds its own (Phase 6: replay QEMU's blobs for exact POST parity) |
| PIIX4 PM (00:01.3) | PM base 0x600, PM timer at 0x608 (SeaBIOS's delay clock: "Using pmtimer, ioport 0x608"), 0x628 | `devices/src/acpi.rs` |
| APM 0xB2/0xB3 | ACPI enable, SMM handshake (skipped with `smm=off`: config 0x5B = 0x02 marks SMM as initialized) | `devices/src/acpi.rs` |
| i8257 DMA | 0x0D, 0xD4, 0xD6, 0xDA (master clear, cascade) | `devices/src/dma.rs`, no transfers |
| ELCR 0x4D0/0x4D1 | written 0x00/0x0C | part of the PIC |
| PCI | full bus scan, BAR sizing and assignment, PAM shadowing | `devices/src/pci.rs`; five functions (host bridge, PIIX3 ISA, PIIX3 IDE, PIIX4 PM, std VGA) |
| std VGA BAR 2 | 256-byte EDID read | `devices/src/vga.rs` |
| std VGA ROM BAR | SeaBIOS copies SeaVGABIOS to 0xC0000 from here | `devices/src/vga.rs` |
| Bochs VBE DISPI (0x1CE/0x1CF) | SeaVGABIOS detects the VBE interface | `devices/src/vga.rs` |
| Local APIC | SeaBIOS's CPU count probe (INIT/SIPI) | the hypervisor's xAPIC |
| i8042 | keyboard/mouse init | `devices/src/ps2.rs` |

The VMM also reports QEMU's `qemu64` CPUID leaves exactly
(`docs/ref/cpuid-qemu64.txt`), including the AMD vendor string: SeaBIOS
decides its physical address width and the e820 layout from them.

## 10. Outside the kernel

- **Demos and optional utilities** may touch hardware directly. Known ones:
  `Demo/Lectures/PCIInterrupts.HC` (IOAPIC), `Demo/Lectures/ScrnMemory.HC`,
  `Demo/Lectures/MiniGrLib.HC`, `Demo/Lectures/GraphicsCPULoad.HC`
  (VGA registers), `Demo/Graphics/Balloon.HC`, `Doc/Comm.HC` (a serial-port
  driver sample on 0x3F8; not loaded by default). They are covered by
  running each demo in the Phase 6 suite. A 16550 UART at 0x3F8 is cheap to
  add later if `Comm.HC` should work.
- **The installer** (`BootHDIns`, `Adam/Opt/Boot/*.HC`) writes an MBR and a
  RedSea or FAT32 partition over ATA and later boots through `INT 13h AH=42h`
  on the hard disk. This is covered by Phase 5's install-and-reboot
  milestone.

## 11. Open questions for the reference traces

These need a real boot of the pinned ISO to settle (see
`tools/qemu-ref/`):

1. ~~Does SeaVGABIOS return `004Fh` for `4F02h`/`0x12`?~~ Yes (§1).
2. ~~Get the exact set and order of port accesses between `KStart16` and
   `TimersInit`.~~ Recorded in `ref/boot/trace.log`. From the VBE mode set
   to the first PIT write of `TimersInit` the guest touches only: VBE DISPI
   (the mode set itself), the VGA registers and DAC (SeaVGABIOS finishing
   mode 12h, then the kernel), ~105k byte writes to 0xA0000 (the kernel
   draws its boot text straight into planar VGA memory), port 0x92, port
   0x61, the RTC (date read), one PIT latch and read, and the local APIC
   (SVR, ID, LDR, DFR). No PCI and no HPET access before `TimersInit`.
3. Measure the VGA write rate per frame, to size the fast-path benefit.
   (Needs a trace that reaches the desktop; QEMU's software CPU hadn't got
   there after 15 minutes.)
4. Check whether anything in the default boot (Adam start-up scripts,
   `HomeSys.HC`) touches hardware beyond this list.
