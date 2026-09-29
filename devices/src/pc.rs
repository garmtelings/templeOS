//! The board: QEMU's `-machine pc,hpet=on,smm=off` (i440FX + PIIX3) reduced
//! to the devices SeaBIOS and TempleOS use, with the port and MMIO maps that
//! connect them. The VMM forwards every exit here.

use std::io::Write;

use crate::acpi::Piix4Pm;
use crate::dma::Dma;
use crate::fwcfg::FwCfg;
use crate::hpet::{Hpet, HPET_BASE};
use crate::ide::{IdeChannel, Media};
use crate::pci::PciBus;
use crate::pic::Pic;
use crate::pit::Pit;
use crate::ps2::Ps2;
use crate::rtc::Rtc;
use crate::vga::Vga;
use crate::{unhandled, GuestMemory, Nanos};

pub struct PcConfig {
    pub ram_size: u64,
    /// SeaVGABIOS, served through the VGA's PCI ROM BAR.
    pub vgabios: &'static [u8],
    /// CD image on the secondary IDE master (QEMU `-cdrom`).
    pub cdrom: Option<&'static [u8]>,
    /// Guest wall-clock time at power-on, in Unix seconds.
    pub rtc_base: i64,
}

pub struct Pc {
    pic: Pic,
    pit: Pit,
    rtc: Rtc,
    ps2: Ps2,
    dma: Dma,
    pci: PciBus,
    pm: Piix4Pm,
    hpet: Hpet,
    fwcfg: FwCfg,
    vga: Vga,
    ide: [IdeChannel; 2],
    /// Port 0x92 (system control port A): bit 1 = A20.
    port92: u8,
    debugcon: Vec<u8>,
    reset_requested: bool,
    kernel_timer: bool,
    trace: Option<Box<dyn Write + Send>>,
}

/// IRQ lines of the board's level-driven sources.
const IRQ_KBD: u8 = 1;
const IRQ_SCI: u8 = 9;
const IRQ_MOUSE: u8 = 12;
const IRQ_IDE: [u8; 2] = [14, 15];

/// Command-block and control-block ports of the two legacy IDE channels.
const IDE_CMD: [u16; 2] = [0x1F0, 0x170];
const IDE_CTL: [u16; 2] = [0x3F6, 0x376];

impl Pc {
    pub fn new(cfg: PcConfig) -> Self {
        let vga = Vga::new(cfg.vgabios);
        let pci = PciBus::with_ram_size(vga.rom().len() as u32, cfg.ram_size);
        let mut pc = Pc {
            pic: Pic::new(),
            pit: Pit::new(),
            rtc: Rtc::new(cfg.rtc_base),
            ps2: Ps2::new(),
            dma: Dma::new(),
            pci,
            pm: Piix4Pm::new(),
            hpet: Hpet::new(),
            fwcfg: fw_cfg(cfg.ram_size),
            vga,
            ide: [
                IdeChannel::new_on_channel(0, None, None),
                IdeChannel::new(cfg.cdrom.map(Media::Cdrom), None),
            ],
            port92: 0,
            debugcon: Vec::new(),
            reset_requested: false,
            kernel_timer: false,
            trace: None,
        };
        pc.init_cmos(cfg.ram_size);
        pc
    }

    /// CMOS configuration bytes as QEMU's pc_cmos_init writes them for this
    /// machine: no floppy or hard disk, boot from CD, one CPU.
    fn init_cmos(&mut self, ram_size: u64) {
        let kib = ram_size / 1024;
        let ext_kib = (kib.saturating_sub(1024)).min(0xFFFF) as u16;
        let above_16m = (ram_size.saturating_sub(16 << 20) / 65536).min(0xFFFF) as u16;
        let bytes = [
            (0x14, 0x06), // equipment: FPU, PS/2 mouse
            (0x15, 0x80), // base memory 640 KiB
            (0x16, 0x02),
            (0x17, ext_kib as u8),
            (0x18, (ext_kib >> 8) as u8),
            (0x30, ext_kib as u8),
            (0x31, (ext_kib >> 8) as u8),
            (0x34, above_16m as u8),
            (0x35, (above_16m >> 8) as u8),
            (0x38, 0x00), // third boot device none, floppy signature check on
            (0x3D, 0x03), // first boot device CD-ROM
            (0x5F, 0x00), // CPUs - 1
        ];
        for (index, val) in bytes {
            self.rtc.set_nvram(index, val);
        }
    }

    /// Record every port and MMIO access, in the format of the QEMU
    /// reference traces (tools/qemu-ref), so the two can be compared.
    pub fn set_trace(&mut self, w: Box<dyn Write + Send>) {
        self.trace = Some(w);
    }

    fn trace(&mut self, write: bool, addr: u64, val: u64, size: u8, name: &str) {
        if let Some(t) = &mut self.trace {
            let dir = if write { "write" } else { "read" };
            let _ = writeln!(
                t,
                "memory_region_ops_{dir} cpu 0 mr 0x0 addr {addr:#x} value {val:#x} size {size} name '{name}'"
            );
        }
    }

    // ---- port I/O ----------------------------------------------------------

    pub fn io_read(&mut self, port: u16, size: u8, now: Nanos) -> u32 {
        let (val, name) = self.io_read_inner(port, size, now);
        self.sync_irqs();
        self.trace(false, port as u64, val as u64, size, name);
        val
    }

    pub fn io_write(&mut self, port: u16, size: u8, val: u32, now: Nanos, mem: &mut dyn GuestMemory) {
        let val = val & mask(size);
        let name = self.io_write_inner(port, size, val, now, mem);
        self.sync_irqs();
        self.trace(true, port as u64, val as u64, size, name);
    }

    fn io_read_inner(&mut self, port: u16, size: u8, now: Nanos) -> (u32, &'static str) {
        if let Some(i) = ide_channel(port) {
            let reg = (port - IDE_CMD[i]) as u8;
            return (self.ide[i].read(reg, size), "ide");
        }
        if let Some(i) = IDE_CTL.iter().position(|&p| p == port) {
            return (self.ide[i].read_alt_status() as u32, "ide");
        }
        if let Some(base) = self.pci.pm_io_base() {
            if (base..base + 0x40).contains(&port) {
                return (self.pm.io_read(port - base, size, now), "piix4-pm");
            }
        }
        match port {
            0xCF8..=0xCFF => (self.pci.io_read(port, size), pci_region(port)),
            0x510..=0x51B => (self.fwcfg.io_read(port, size), fwcfg_region(port)),
            0x1CE | 0x1CF | 0x1D0 => (self.vga.dispi_read(port) as u32, "vbe"),
            _ => {
                let mut val = 0u32;
                let mut name = "";
                for i in 0..size.min(4) {
                    let (b, n) = self.byte_read(port.wrapping_add(i as u16), now);
                    val |= (b as u32) << (8 * i);
                    if i == 0 {
                        name = n;
                    }
                }
                (val, name)
            }
        }
    }

    fn io_write_inner(
        &mut self,
        port: u16,
        size: u8,
        val: u32,
        now: Nanos,
        mem: &mut dyn GuestMemory,
    ) -> &'static str {
        if let Some(i) = ide_channel(port) {
            self.ide[i].write((port - IDE_CMD[i]) as u8, size, val);
            return "ide";
        }
        if let Some(i) = IDE_CTL.iter().position(|&p| p == port) {
            self.ide[i].write_device_control(val as u8);
            return "ide";
        }
        if let Some(base) = self.pci.pm_io_base() {
            if (base..base + 0x40).contains(&port) {
                self.pm.io_write(port - base, size, val, now);
                return "piix4-pm";
            }
        }
        match port {
            0xCF8..=0xCFF => {
                self.pci.io_write(port, size, val);
                if self.pci.take_reset_request() {
                    self.reset_requested = true;
                }
                pci_region(port)
            }
            0x510..=0x51B => {
                self.fwcfg.io_write(port, size, val, mem);
                fwcfg_region(port)
            }
            0x1CE | 0x1CF | 0x1D0 => {
                self.vga.dispi_write(port, val as u16);
                "vbe"
            }
            _ => {
                let mut name = "";
                for i in 0..size.min(4) {
                    let n = self.byte_write(port.wrapping_add(i as u16), (val >> (8 * i)) as u8, now);
                    if i == 0 {
                        name = n;
                    }
                }
                name
            }
        }
    }

    /// Devices with byte-wide registers. Wider accesses are split into
    /// consecutive byte accesses, as QEMU's portio layer does.
    fn byte_read(&mut self, port: u16, now: Nanos) -> (u8, &'static str) {
        match port {
            p if Dma::handles(p) => (self.dma.read(p), "dma"),
            0x20 | 0x21 | 0xA0 | 0xA1 => (self.pic.read(port), "pic"),
            0x4D0 | 0x4D1 => (self.pic.read(port), "elcr"),
            0x40..=0x43 => (self.pit.read(port, now), "pit"),
            0x61 => (self.pit.read_port61(now), "pcspk"),
            0x60 | 0x64 => (self.ps2.read(port), "i8042"),
            0x70 => (self.rtc.read(port, now), "rtc-index"),
            0x71 => (self.rtc.read(port, now), "rtc"),
            0x80 => (0xFF, "ioport80"),
            0x92 => (self.port92, "port92"),
            0xB2 | 0xB3 => (self.pm.apm_read(port), "apm-io"),
            0x3B0..=0x3DF => (self.vga.io_read(port), "vga"),
            // QEMU's isa-debugcon reads back its configured value.
            0x402 => (0xE9, "isa-debugcon"),
            _ => {
                unhandled("io read", port as u64, 1, None);
                (0xFF, "unassigned")
            }
        }
    }

    fn byte_write(&mut self, port: u16, val: u8, now: Nanos) -> &'static str {
        match port {
            p if Dma::handles(p) => {
                self.dma.write(p, val);
                "dma"
            }
            0x20 | 0x21 | 0xA0 | 0xA1 => {
                self.pic.write(port, val);
                "pic"
            }
            0x4D0 | 0x4D1 => {
                self.pic.write(port, val);
                "elcr"
            }
            0x40..=0x43 => {
                // TimersInit loads channel 0 with 0x04A8; SeaBIOS never writes 0xA8 there.
                if port == 0x40 && val == 0xA8 {
                    self.kernel_timer = true;
                }
                self.pit.write(port, val, now);
                // A counter load can change the IRQ0 level; deliver it now.
                let pic = &mut self.pic;
                self.pit.advance(now, &mut |level| pic.set_irq(0, level));
                "pit"
            }
            0x61 => {
                self.pit.write_port61(val, now);
                "pcspk"
            }
            0x60 | 0x64 => {
                self.ps2.write(port, val);
                if self.ps2.take_reset_request() {
                    self.reset_requested = true;
                }
                "i8042"
            }
            0x70 | 0x71 => {
                self.rtc.write(port, val, now);
                if port == 0x70 { "rtc-index" } else { "rtc" }
            }
            0x80 => "ioport80",
            0x92 => {
                // Bit 0 rising = fast reset; bit 1 = A20 (always on with WHPX:
                // the gate can't be emulated, and QEMU resets with A20 enabled).
                if val & 1 != 0 && self.port92 & 1 == 0 {
                    self.reset_requested = true;
                }
                self.port92 = val & 0x02;
                "port92"
            }
            0xB2 | 0xB3 => {
                self.pm.apm_write(port, val);
                "apm-io"
            }
            0x3B0..=0x3DF => {
                self.vga.io_write(port, val);
                "vga"
            }
            0x402 => {
                self.debugcon.push(val);
                "isa-debugcon"
            }
            _ => {
                unhandled("io write", port as u64, 1, Some(val as u64));
                "unassigned"
            }
        }
    }

    // ---- MMIO ----------------------------------------------------------------

    pub fn mmio_read(&mut self, gpa: u64, size: u8, now: Nanos) -> u64 {
        let (val, name) = match self.mmio_target(gpa) {
            Mmio::Hpet(off) => (self.hpet.read(off, size, now), "hpet"),
            Mmio::VgaLfb(off) => (le_read(self.vga.vram(), off, size), "vga.vram"),
            Mmio::VgaBar2(off) => (self.vga.bar2_read(off, size), "vga.mmio"),
            Mmio::VgaRom(off) => (le_read(self.vga.rom(), off, size), "vga.rom"),
            Mmio::None => {
                unhandled("mmio read", gpa, size, None);
                // QEMU's unassigned memory reads as 0.
                (0, "unassigned")
            }
        };
        self.trace(false, gpa, val, size, name);
        val
    }

    pub fn mmio_write(&mut self, gpa: u64, size: u8, val: u64, now: Nanos) {
        let name = match self.mmio_target(gpa) {
            Mmio::Hpet(off) => {
                self.hpet.write(off, size, val, now);
                "hpet"
            }
            Mmio::VgaLfb(off) => {
                le_write(self.vga.vram_mut(), off, size, val);
                "vga.vram"
            }
            Mmio::VgaBar2(off) => {
                self.vga.bar2_write(off, size, val);
                "vga.mmio"
            }
            Mmio::VgaRom(_) => "vga.rom",
            Mmio::None => {
                unhandled("mmio write", gpa, size, Some(val));
                "unassigned"
            }
        };
        self.trace(true, gpa, val, size, name);
    }

    fn mmio_target(&self, gpa: u64) -> Mmio {
        if (HPET_BASE..HPET_BASE + 0x400).contains(&gpa) {
            return Mmio::Hpet((gpa - HPET_BASE) as u32);
        }
        let within = |base: Option<u32>, size: u64| {
            base.map(|b| b as u64).filter(|&b| gpa >= b && gpa < b + size).map(|b| (gpa - b) as u32)
        };
        if let Some(off) = within(self.pci.vga_bar(0), crate::vga::VRAM_SIZE as u64) {
            return Mmio::VgaLfb(off);
        }
        if let Some(off) = within(self.pci.vga_bar(2), 0x1000) {
            return Mmio::VgaBar2(off);
        }
        if let Some(off) = within(self.pci.vga_rom_base(), self.vga.rom().len() as u64) {
            return Mmio::VgaRom(off);
        }
        Mmio::None
    }

    // ---- time and interrupts ---------------------------------------------------

    /// Bring timers up to `now`, raising any interrupts that became due.
    pub fn poll(&mut self, now: Nanos) {
        let pic = &mut self.pic;
        self.pit.advance(now, &mut |level| pic.set_irq(0, level));
        self.sync_irqs();
    }

    /// When the next timer interrupt is due, if any.
    pub fn next_deadline(&self) -> Option<Nanos> {
        self.pit.next_event()
    }

    /// Feed the level-driven interrupt sources into the PIC. The PIC does
    /// its own edge detection, so repeating an unchanged level is harmless.
    fn sync_irqs(&mut self) {
        // Reading one queued PS/2 byte while more wait drops and re-raises
        // the line within that access (as QEMU does); the PIC must see the edge.
        if self.ps2.take_irq1_edge() {
            self.pic.set_irq(IRQ_KBD, false);
        }
        if self.ps2.take_irq12_edge() {
            self.pic.set_irq(IRQ_MOUSE, false);
        }
        self.pic.set_irq(IRQ_KBD, self.ps2.irq1());
        self.pic.set_irq(IRQ_MOUSE, self.ps2.irq12());
        self.pic.set_irq(IRQ_SCI, self.pm.sci_level());
        for (ch, irq) in self.ide.iter().zip(IRQ_IDE) {
            self.pic.set_irq(irq, ch.irq_level());
        }
    }

    pub fn has_interrupt(&self) -> bool {
        self.pic.has_interrupt()
    }

    /// The CPU's interrupt-acknowledge cycle: returns the vector to deliver.
    pub fn acknowledge_interrupt(&mut self) -> u8 {
        let v = self.pic.acknowledge();
        self.sync_irqs();
        v
    }

    // ---- misc ----------------------------------------------------------------

    /// Bytes the guest wrote to the SeaBIOS debug console (port 0x402).
    pub fn take_debugcon(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.debugcon)
    }

    /// True once after the guest asked for a platform reset.
    pub fn take_reset_request(&mut self) -> bool {
        std::mem::take(&mut self.reset_requested)
    }

    /// The kernel's TimersInit has programmed the PIT (a Phase 2 milestone).
    pub fn kernel_timer_programmed(&self) -> bool {
        self.kernel_timer
    }

    pub fn vga(&self) -> &Vga {
        &self.vga
    }

    pub fn ps2_mut(&mut self) -> &mut Ps2 {
        &mut self.ps2
    }
}

enum Mmio {
    Hpet(u32),
    VgaLfb(u32),
    VgaBar2(u32),
    VgaRom(u32),
    None,
}

fn ide_channel(port: u16) -> Option<usize> {
    IDE_CMD.iter().position(|&base| (base..base + 8).contains(&port))
}

fn pci_region(port: u16) -> &'static str {
    match port {
        0xCF8 => "pci-conf-idx",
        0xCF9 => "piix-reset-control",
        _ => "pci-conf-data",
    }
}

fn fwcfg_region(port: u16) -> &'static str {
    if port < 0x514 {
        "fwcfg"
    } else {
        "fwcfg.dma"
    }
}

fn mask(size: u8) -> u32 {
    if size >= 4 {
        u32::MAX
    } else {
        (1 << (8 * size)) - 1
    }
}

fn le_read(buf: &[u8], off: u32, size: u8) -> u64 {
    let mut v = 0u64;
    for i in 0..size as usize {
        v |= (*buf.get(off as usize + i).unwrap_or(&0xFF) as u64) << (8 * i);
    }
    v
}

fn le_write(buf: &mut [u8], off: u32, size: u8, val: u64) {
    for i in 0..size as usize {
        if let Some(b) = buf.get_mut(off as usize + i) {
            *b = (val >> (8 * i)) as u8;
        }
    }
}

/// fw_cfg as QEMU's pc machine provides it (FwCfg::new adds the entries
/// QEMU always has: UUID, NOGRAPHIC, BOOT_MENU, etc/boot-fail-wait), minus the ACPI and SMBIOS
/// tables (SeaBIOS builds its own when they're absent; TempleOS uses
/// neither). The e820 map matches QEMU's for this CPU: the AMD
/// HyperTransport hole is reserved because the qemu64 model is an AMD CPU
/// with 40 physical address bits.
fn fw_cfg(ram_size: u64) -> FwCfg {
    let mut f = FwCfg::new();
    f.add_bytes(0x03, ram_size.to_le_bytes().to_vec()); // RAM_SIZE
    f.add_bytes(0x05, 1u16.to_le_bytes().to_vec()); // NB_CPUS
    f.add_bytes(0x0F, 1u16.to_le_bytes().to_vec()); // MAX_CPUS
    let mut e820 = Vec::new();
    for (addr, len, kind) in [(0xFD_0000_0000u64, 0x3_0000_0000u64, 2u32), (0, ram_size, 1)] {
        e820.extend_from_slice(&addr.to_le_bytes());
        e820.extend_from_slice(&len.to_le_bytes());
        e820.extend_from_slice(&kind.to_le_bytes());
    }
    f.add_file("etc/e820", e820);
    f
}
