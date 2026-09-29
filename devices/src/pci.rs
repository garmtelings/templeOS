//! PCI bus 0 of the i440FX + PIIX3 machine and configuration mechanism #1.
//!
//! The bus holds exactly the functions of the reference machine
//! (`-machine pc -nodefaults -device VGA`):
//!
//! | BDF     | Device                         | IDs       |
//! |---------|--------------------------------|-----------|
//! | 00:00.0 | i440FX host bridge             | 8086:1237 |
//! | 00:01.0 | PIIX3 PCI-to-ISA bridge        | 8086:7000 |
//! | 00:01.1 | PIIX3 IDE (compat mode)        | 8086:7010 |
//! | 00:01.3 | PIIX4 ACPI / power management  | 8086:7113 |
//! | 00:02.0 | Bochs/QEMU standard VGA        | 1234:1111 |
//!
//! Config space follows QEMU's `hw/pci/pci.c`: every register byte has a
//! write mask and a write-1-to-clear mask, BARs are sized through their write
//! masks, and device-specific registers (0x40-0xFF) are fully writable. The
//! bus only stores config space; the board asks it for the decoded state it
//! routes on (PM I/O base, BMDMA base, VGA BARs, PAM, SMRAM).

/// Size of a conventional PCI function's config space.
pub const CONFIG_SPACE_SIZE: usize = 256;

/// Bus/device/function numbers as `bus << 8 | device << 3 | function`.
pub const fn bdf(bus: u8, device: u8, function: u8) -> u16 {
    ((bus as u16) << 8) | ((device as u16 & 0x1F) << 3) | (function as u16 & 7)
}

pub const HOST_BRIDGE_BDF: u16 = bdf(0, 0, 0);
pub const ISA_BRIDGE_BDF: u16 = bdf(0, 1, 0);
pub const IDE_BDF: u16 = bdf(0, 1, 1);
pub const PM_BDF: u16 = bdf(0, 1, 3);
pub const VGA_BDF: u16 = bdf(0, 2, 0);

/// Config registers of the standard type-0 header.
pub mod reg {
    pub const VENDOR_ID: u8 = 0x00;
    pub const DEVICE_ID: u8 = 0x02;
    pub const COMMAND: u8 = 0x04;
    pub const STATUS: u8 = 0x06;
    pub const REVISION: u8 = 0x08;
    pub const CLASS_PROG: u8 = 0x09;
    pub const CACHE_LINE_SIZE: u8 = 0x0C;
    pub const HEADER_TYPE: u8 = 0x0E;
    pub const BAR0: u8 = 0x10;
    pub const SUBSYSTEM_VENDOR_ID: u8 = 0x2C;
    pub const SUBSYSTEM_ID: u8 = 0x2E;
    pub const ROM_ADDRESS: u8 = 0x30;
    pub const INTERRUPT_LINE: u8 = 0x3C;
    pub const INTERRUPT_PIN: u8 = 0x3D;
}

/// Command register bits.
pub mod cmd {
    pub const IO: u16 = 0x0001;
    pub const MEMORY: u16 = 0x0002;
    pub const MASTER: u16 = 0x0004;
    pub const SERR: u16 = 0x0100;
    pub const INTX_DISABLE: u16 = 0x0400;
}

/// Status register bits.
pub mod status {
    pub const FAST_BACK: u16 = 0x0080;
    pub const DEVSEL_MEDIUM: u16 = 0x0200;
    pub const SIG_SYSTEM_ERROR: u16 = 0x4000;
    pub const DETECTED_PARITY: u16 = 0x8000;
    pub const PARITY: u16 = 0x0100;
    pub const SIG_TARGET_ABORT: u16 = 0x0800;
    pub const REC_TARGET_ABORT: u16 = 0x1000;
    pub const REC_MASTER_ABORT: u16 = 0x2000;
}

/// i440FX host bridge registers.
pub const I440FX_RAM_SIZE: u8 = 0x57;
pub const I440FX_PAM: u8 = 0x59;
pub const I440FX_PAM_COUNT: usize = 7;
pub const I440FX_SMRAM: u8 = 0x72;

/// PIIX3 ISA bridge PIRQ route control registers (PIRQA..PIRQD).
pub const PIIX_PIRQC: u8 = 0x60;

/// PIIX4 power-management function registers.
pub const PIIX4_PMBA: u8 = 0x40;
pub const PIIX4_DEVACTB: u8 = 0x58;
pub const PIIX4_PMREGMISC: u8 = 0x80;
pub const PIIX4_SMBBA: u8 = 0x90;
pub const PIIX4_SMBHSTCFG: u8 = 0xD2;

/// PIIX3 reset control register.
pub const RCR_PORT: u16 = 0xCF9;
pub const CONFIG_ADDRESS_PORT: u16 = 0xCF8;
pub const CONFIG_DATA_PORT: u16 = 0xCFC;

/// Index of the expansion ROM in BAR tables (BARs 0-5 are the header BARs).
pub const ROM_SLOT: usize = 6;

/// What a BAR decodes, which fixes its low type bits and how it is sized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    Io,
    Mem32,
    Mem32Prefetch,
    /// Expansion ROM (register 0x30): bit 0 is the ROM enable bit.
    Rom,
}

impl BarKind {
    fn type_bits(self) -> u32 {
        match self {
            BarKind::Io => 0x1,
            BarKind::Mem32 | BarKind::Rom => 0x0,
            BarKind::Mem32Prefetch => 0x8,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Bar {
    size: u32,
    kind: BarKind,
}

/// Config space of one PCI function: 256 bytes plus QEMU-style write mask and
/// write-1-to-clear mask.
///
/// Guest writes (`write`) only change bits set in the write mask, and clear
/// bits that are written as 1 where the W1C mask is set. The `set_*` methods
/// bypass the masks and are for building reset values.
#[derive(Clone)]
pub struct PciConfig {
    bytes: [u8; CONFIG_SPACE_SIZE],
    wmask: [u8; CONFIG_SPACE_SIZE],
    w1cmask: [u8; CONFIG_SPACE_SIZE],
    bars: [Option<Bar>; 7],
}

impl PciConfig {
    /// A function with QEMU's default masks (`pci_init_wmask`,
    /// `pci_init_w1cmask`) and default subsystem ID 1AF4:1100.
    pub fn new(vendor_id: u16, device_id: u16, class_code: u32, revision: u8) -> Self {
        let mut c = PciConfig {
            bytes: [0; CONFIG_SPACE_SIZE],
            wmask: [0; CONFIG_SPACE_SIZE],
            w1cmask: [0; CONFIG_SPACE_SIZE],
            bars: [None; 7],
        };
        c.set_u16(reg::VENDOR_ID, vendor_id);
        c.set_u16(reg::DEVICE_ID, device_id);
        c.set_u8(reg::REVISION, revision);
        c.set_u8(reg::CLASS_PROG, class_code as u8);
        c.set_u16(reg::CLASS_PROG + 1, (class_code >> 8) as u16);
        c.set_u16(reg::SUBSYSTEM_VENDOR_ID, 0x1AF4);
        c.set_u16(reg::SUBSYSTEM_ID, 0x1100);

        c.wmask[reg::CACHE_LINE_SIZE as usize] = 0xFF;
        c.wmask[reg::INTERRUPT_LINE as usize] = 0xFF;
        let cmd_mask = cmd::IO | cmd::MEMORY | cmd::MASTER | cmd::INTX_DISABLE | cmd::SERR;
        c.set_mask16(reg::COMMAND, cmd_mask);
        c.wmask[0x40..].fill(0xFF);
        let w1c = status::PARITY
            | status::SIG_TARGET_ABORT
            | status::REC_TARGET_ABORT
            | status::REC_MASTER_ABORT
            | status::SIG_SYSTEM_ERROR
            | status::DETECTED_PARITY;
        c.w1cmask[reg::STATUS as usize..][..2].copy_from_slice(&w1c.to_le_bytes());
        c
    }

    fn set_mask16(&mut self, r: u8, mask: u16) {
        self.wmask[r as usize..][..2].copy_from_slice(&mask.to_le_bytes());
    }

    /// Declare BAR `index` (0-5, or [`ROM_SLOT`]) of `size` bytes, a power of
    /// two. Its address bits become writable, so writing all-ones reads back
    /// `!(size - 1) | type`, as in QEMU's `pci_register_bar`.
    pub fn register_bar(&mut self, index: usize, size: u32, kind: BarKind) {
        assert!(size.is_power_of_two(), "BAR size {size:#x} is not a power of two");
        assert_eq!(index == ROM_SLOT, kind == BarKind::Rom, "ROM BAR must use ROM_SLOT");
        let mut wmask = !(size - 1);
        if kind == BarKind::Rom {
            wmask |= 1;
        }
        let r = Self::bar_reg(index);
        self.bars[index] = Some(Bar { size, kind });
        self.set_u32(r, kind.type_bits());
        self.wmask[r as usize..][..4].copy_from_slice(&wmask.to_le_bytes());
    }

    fn bar_reg(index: usize) -> u8 {
        if index == ROM_SLOT {
            reg::ROM_ADDRESS
        } else {
            assert!(index < 6);
            reg::BAR0 + 4 * index as u8
        }
    }

    /// Guest-visible read of `size` bytes at `reg`, clipped at the end of
    /// config space like QEMU (`MIN(len, limit - addr)`).
    pub fn read(&self, reg: u8, size: u8) -> u32 {
        let start = reg as usize;
        let len = (size as usize).min(CONFIG_SPACE_SIZE - start);
        let mut v = 0u32;
        for (i, b) in self.bytes[start..start + len].iter().enumerate() {
            v |= (*b as u32) << (8 * i);
        }
        v
    }

    /// Guest write of `size` bytes at `reg`, through the write and W1C masks
    /// (`pci_default_write_config`).
    pub fn write(&mut self, reg: u8, size: u8, val: u32) {
        let start = reg as usize;
        let len = (size as usize).min(CONFIG_SPACE_SIZE - start);
        for i in 0..len {
            let a = start + i;
            let v = (val >> (8 * i)) as u8;
            let (wm, w1c) = (self.wmask[a], self.w1cmask[a]);
            self.bytes[a] = (self.bytes[a] & !wm) | (v & wm);
            self.bytes[a] &= !(v & w1c);
        }
    }

    pub fn u8(&self, reg: u8) -> u8 {
        self.bytes[reg as usize]
    }

    pub fn u16(&self, reg: u8) -> u16 {
        self.read(reg, 2) as u16
    }

    pub fn u32(&self, reg: u8) -> u32 {
        self.read(reg, 4)
    }

    pub fn set_u8(&mut self, reg: u8, v: u8) {
        self.bytes[reg as usize] = v;
    }

    pub fn set_u16(&mut self, reg: u8, v: u16) {
        self.bytes[reg as usize..][..2].copy_from_slice(&v.to_le_bytes());
    }

    pub fn set_u32(&mut self, reg: u8, v: u32) {
        self.bytes[reg as usize..][..4].copy_from_slice(&v.to_le_bytes());
    }

    /// Mask of the bits a guest write can change at `reg`.
    pub fn write_mask(&self, reg: u8) -> u8 {
        self.wmask[reg as usize]
    }

    pub fn command(&self) -> u16 {
        self.u16(reg::COMMAND)
    }

    /// The generic part of a PCI reset (`pci_do_device_reset`): clear the
    /// writable command/status bits, the interrupt line and cache line size,
    /// and return every BAR to its bare type bits.
    pub fn reset(&mut self) {
        for r in [reg::COMMAND, reg::STATUS] {
            let m = u16::from_le_bytes([self.wmask[r as usize], self.wmask[r as usize + 1]])
                | u16::from_le_bytes([self.w1cmask[r as usize], self.w1cmask[r as usize + 1]]);
            let v = self.u16(r) & !m;
            self.set_u16(r, v);
        }
        let il = reg::INTERRUPT_LINE as usize;
        self.bytes[il] &= !(self.wmask[il] | self.w1cmask[il]);
        self.bytes[reg::CACHE_LINE_SIZE as usize] = 0;
        for i in 0..self.bars.len() {
            if let Some(bar) = self.bars[i] {
                self.set_u32(Self::bar_reg(i), bar.kind.type_bits());
            }
        }
    }

    /// Where BAR `index` is currently mapped, following QEMU's
    /// `pci_bar_address`: `None` when the BAR is not implemented, its decode
    /// is disabled in the command register (or ROM enable is clear), it is
    /// at address 0, or it would wrap/extend past 4 GiB (which also rejects
    /// the all-ones sizing pattern).
    pub fn bar_address(&self, index: usize) -> Option<u32> {
        let bar = self.bars[index]?;
        let cmd = self.command();
        let raw = self.u32(Self::bar_reg(index));
        let enabled = match bar.kind {
            BarKind::Io => cmd & cmd::IO != 0,
            BarKind::Rom => cmd & cmd::MEMORY != 0 && raw & 1 != 0,
            BarKind::Mem32 | BarKind::Mem32Prefetch => cmd & cmd::MEMORY != 0,
        };
        if !enabled {
            return None;
        }
        let base = (raw & !(bar.size - 1)) as u64;
        let last = base + bar.size as u64 - 1;
        if base == 0 || last >= u32::MAX as u64 {
            return None;
        }
        Some(base as u32)
    }

    /// Size of BAR `index`, if implemented.
    pub fn bar_size(&self, index: usize) -> Option<u32> {
        self.bars[index].map(|b| b.size)
    }
}

/// Positions of the functions in [`PciBus::functions`].
const HOST: usize = 0;
const ISA: usize = 1;
const IDE: usize = 2;
const PM: usize = 3;
const VGA: usize = 4;

/// The machine's PCI bus 0 and config mechanism #1 at 0xCF8 (32-bit address
/// latch) / 0xCFC-0xCFF (data, 1/2/4 bytes at the matching offset).
/// Absent functions read all-ones. Also owns the PIIX3 reset control register
/// at 0xCF9 (byte access).
///
/// Port behavior matches QEMU's `pci_host.c` under TCG:
/// - 0xCF8 is written only by 32-bit writes at 0xCF8; reads of any size at
///   0xCF8/0xCFA/0xCFB return the latch (truncated to the access size).
/// - Data accesses use `latch | (port & 3)` as the address, without masking
///   the latch's low bits, and return all-ones while latch bit 31 is clear.
/// - Misaligned accesses (e.g. a word at 0xCFD) are invalid: reads return 0,
///   writes are dropped.
pub struct PciBus {
    config_address: u32,
    functions: [(u16, PciConfig); 5],
    rcr: u8,
    reset_requested: bool,
    vga_rom_size: u32,
    ram_size: u64,
}

impl PciBus {
    /// `vga_rom_size`: power of two, sizes the VGA ROM BAR (0 = no ROM).
    pub fn new(vga_rom_size: u32) -> Self {
        Self::with_ram_size(vga_rom_size, 1 << 30)
    }

    /// Like [`PciBus::new`], with the guest RAM size the i440FX reports in
    /// register 0x57 (RAM size in 8 MiB units, capped at 255; QEMU sets it
    /// for coreboot). [`PciBus::new`] assumes the reference 1 GiB.
    pub fn with_ram_size(vga_rom_size: u32, ram_size: u64) -> Self {
        assert!(
            vga_rom_size == 0 || vga_rom_size.is_power_of_two(),
            "VGA ROM size must be a power of two"
        );
        let mut host = PciConfig::new(0x8086, 0x1237, 0x06_00_00, 0x02);
        host.set_u8(I440FX_RAM_SIZE, (ram_size >> 23).min(255) as u8);
        host.set_u8(I440FX_SMRAM, 0x02);

        let mut isa = PciConfig::new(0x8086, 0x7000, 0x06_01_00, 0x00);
        isa.set_u8(reg::HEADER_TYPE, 0x80);
        // piix_reset(); the command register it sets is cleared again by the
        // generic reset below, exactly as in QEMU.
        isa.set_u16(reg::COMMAND, 0x0007);
        isa.set_u16(reg::STATUS, status::DEVSEL_MEDIUM);
        for (r, v) in [
            (0x4C, 0x4D),
            (0x4E, 0x03),
            (0x60, 0x80),
            (0x61, 0x80),
            (0x62, 0x80),
            (0x63, 0x80),
            (0x69, 0x02),
            (0x70, 0x80),
            (0x76, 0x0C),
            (0x77, 0x0C),
            (0x78, 0x02),
            (0xA0, 0x08),
            (0xA8, 0x0F),
        ] {
            isa.set_u8(r, v);
        }

        let mut ide = PciConfig::new(0x8086, 0x7010, 0x01_01_80, 0x00);
        ide.register_bar(4, 16, BarKind::Io);
        ide.set_u16(reg::STATUS, status::DEVSEL_MEDIUM | status::FAST_BACK);

        let mut pm = PciConfig::new(0x8086, 0x7113, 0x06_80_00, 0x03);
        pm.set_u16(reg::STATUS, status::DEVSEL_MEDIUM | status::FAST_BACK);
        pm.set_u8(reg::INTERRUPT_PIN, 1);
        pm.set_u8(PIIX4_PMBA, 0x01);
        // smm=off: QEMU marks SMM as already initialised (DEVACTB.APMC_EN),
        // so SeaBIOS skips its SMM setup.
        pm.set_u8(PIIX4_DEVACTB + 3, 0x02);

        let mut vga = PciConfig::new(0x1234, 0x1111, 0x03_00_00, 0x02);
        vga.register_bar(0, 16 << 20, BarKind::Mem32Prefetch);
        vga.register_bar(2, 4096, BarKind::Mem32);
        if vga_rom_size != 0 {
            vga.register_bar(ROM_SLOT, vga_rom_size, BarKind::Rom);
        }

        let mut functions = [
            (HOST_BRIDGE_BDF, host),
            (ISA_BRIDGE_BDF, isa),
            (IDE_BDF, ide),
            (PM_BDF, pm),
            (VGA_BDF, vga),
        ];
        for (_, f) in &mut functions {
            f.reset();
        }
        PciBus {
            config_address: 0,
            functions,
            rcr: 0,
            reset_requested: false,
            vga_rom_size,
            ram_size,
        }
    }

    /// Return every function to its power-on state (a platform reset).
    pub fn reset(&mut self) {
        *self = Self::with_ram_size(self.vga_rom_size, self.ram_size);
    }

    /// Config space of the function at `bdf`, if present.
    pub fn function(&self, bdf: u16) -> Option<&PciConfig> {
        self.functions.iter().find(|(b, _)| *b == bdf).map(|(_, c)| c)
    }

    fn function_mut(&mut self, bdf: u16) -> Option<&mut PciConfig> {
        self.functions.iter_mut().find(|(b, _)| *b == bdf).map(|(_, c)| c)
    }

    fn cfg(&self, index: usize) -> &PciConfig {
        &self.functions[index].1
    }

    pub fn io_read(&mut self, port: u16, size: u8) -> u32 {
        let mask = size_mask(size);
        match port {
            RCR_PORT => {
                // The one-byte register overlays the latch; wider reads are
                // split into byte reads that each return RCR.
                (0..size).fold(0, |v, i| v | (self.rcr as u32) << (8 * i))
            }
            0xCF8..=0xCFB => {
                if !aligned(port, size) {
                    return 0;
                }
                self.config_address & mask
            }
            0xCFC..=0xCFF => {
                if !aligned(port, size) {
                    return 0;
                }
                if self.config_address & 0x8000_0000 == 0 {
                    return mask;
                }
                let addr = self.config_address | (port as u32 & 3);
                self.config_read(addr_bdf(addr), addr as u8, size)
            }
            _ => {
                crate::unhandled("pci io read", port as u64, size, None);
                mask
            }
        }
    }

    pub fn io_write(&mut self, port: u16, size: u8, val: u32) {
        match port {
            RCR_PORT => {
                for i in 0..size {
                    self.rcr_write((val >> (8 * i)) as u8);
                }
            }
            0xCF8..=0xCFB => {
                if port == CONFIG_ADDRESS_PORT && size == 4 {
                    self.config_address = val;
                }
            }
            0xCFC..=0xCFF => {
                if !aligned(port, size) || self.config_address & 0x8000_0000 == 0 {
                    return;
                }
                let addr = self.config_address | (port as u32 & 3);
                self.config_write(addr_bdf(addr), addr as u8, size, val);
            }
            _ => crate::unhandled("pci io write", port as u64, size, Some(val as u64)),
        }
    }

    /// QEMU `rcr_write`: bit 2 requests a reset (without latching the
    /// value); otherwise only the reset-type bit 1 is kept.
    fn rcr_write(&mut self, val: u8) {
        if val & 4 != 0 {
            self.reset_requested = true;
        } else {
            self.rcr = val & 2;
        }
    }

    /// Config read as the guest sees it through 0xCFC; absent functions read
    /// all-ones. The result is truncated to `size` bytes.
    pub fn config_read(&mut self, bdf: u16, reg: u8, size: u8) -> u32 {
        match self.function(bdf) {
            Some(f) => f.read(reg, size),
            None => size_mask(size),
        }
    }

    pub fn config_write(&mut self, bdf: u16, reg: u8, size: u8, val: u32) {
        if let Some(f) = self.function_mut(bdf) {
            f.write(reg, size, val);
        }
    }

    /// PIIX4 PM I/O block base (PMBA & 0xFFC0) when PMREGMISC bit 0 enables
    /// it. QEMU maps the block whenever it is enabled, even at base 0.
    pub fn pm_io_base(&self) -> Option<u16> {
        let pm = self.cfg(PM);
        (pm.u8(PIIX4_PMREGMISC) & 1 != 0).then(|| pm.u32(PIIX4_PMBA) as u16 & 0xFFC0)
    }

    /// PIIX4 SMBus I/O base (SMBBA & 0xFFC0) when SMBHSTCFG bit 0 enables it.
    pub fn smbus_io_base(&self) -> Option<u16> {
        let pm = self.cfg(PM);
        (pm.u8(PIIX4_SMBHSTCFG) & 1 != 0).then(|| pm.u32(PIIX4_SMBBA) as u16 & 0xFFC0)
    }

    /// PIIX3 IDE bus-master DMA base (BAR4) when I/O decode is enabled.
    /// A BAR programmed above 0xFFFF is outside the I/O space and reported as
    /// `None`.
    pub fn ide_bmdma_base(&self) -> Option<u16> {
        self.cfg(IDE).bar_address(4).and_then(|a| u16::try_from(a).ok())
    }

    /// VGA BAR0 (16 MiB linear framebuffer) or BAR2 (4 KiB MMIO) when memory
    /// decode is enabled and the BAR is assigned.
    pub fn vga_bar(&self, index: usize) -> Option<u32> {
        if index >= 6 {
            return None;
        }
        self.cfg(VGA).bar_address(index)
    }

    /// VGA expansion ROM base when memory decode and the ROM enable bit are set.
    pub fn vga_rom_base(&self) -> Option<u32> {
        self.cfg(VGA).bar_address(ROM_SLOT)
    }

    /// i440FX PAM registers 0x59..=0x5F.
    pub fn pam(&self) -> [u8; 7] {
        let h = self.cfg(HOST);
        std::array::from_fn(|i| h.u8(I440FX_PAM + i as u8))
    }

    /// i440FX SMRAM control register 0x72.
    pub fn smram(&self) -> u8 {
        self.cfg(HOST).u8(I440FX_SMRAM)
    }

    /// ISA IRQ that PIIX3 PIRQ line `n` (0 = PIRQA .. 3 = PIRQD) is routed to,
    /// or `None` when its route is disabled (bit 7). No function on this bus
    /// raises INTx, so the board needs this only for completeness.
    pub fn pirq_route(&self, n: usize) -> Option<u8> {
        assert!(n < 4);
        let r = self.cfg(ISA).u8(PIIX_PIRQC + n as u8);
        (r & 0x80 == 0).then_some(r & 0x0F)
    }

    /// True once after a write to 0xCF9 with bit 2 set (QEMU rcr semantics).
    pub fn take_reset_request(&mut self) -> bool {
        std::mem::take(&mut self.reset_requested)
    }
}

fn size_mask(size: u8) -> u32 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => 0xFFFF_FFFF,
    }
}

fn aligned(port: u16, size: u8) -> bool {
    matches!(size, 1 | 2 | 4) && port as u32 & (size as u32 - 1) == 0
}

/// BDF of a config mechanism #1 address (bus 23:16, devfn 15:8).
fn addr_bdf(addr: u32) -> u16 {
    (addr >> 8) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
    }

    fn vga_rom_size() -> u32 {
        let len = std::fs::metadata(workspace_root().join("payload/vgabios.bin"))
            .expect("payload/vgabios.bin")
            .len();
        (len as u32).next_power_of_two()
    }

    fn parse_bdf(s: &str) -> u16 {
        let (bus, rest) = s.split_once(':').unwrap();
        let (dev, func) = rest.split_once('.').unwrap();
        bdf(
            u8::from_str_radix(bus, 16).unwrap(),
            u8::from_str_radix(dev, 16).unwrap(),
            func.parse().unwrap(),
        )
    }

    fn hex(s: &str) -> u64 {
        u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
    }

    fn addr(bdf: u16, reg: u8) -> u32 {
        0x8000_0000 | (bdf as u32) << 8 | reg as u32
    }

    #[test]
    fn reset_values_match_reference() {
        let path = workspace_root().join("docs/ref/pci-config-reset.txt");
        let text = std::fs::read_to_string(path).unwrap();
        let mut bus = PciBus::new(vga_rom_size());
        let mut checked = 0;
        for line in text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            let f: Vec<&str> = line.split_whitespace().collect();
            let (b, r, size, want) = (parse_bdf(f[0]), hex(f[1]) as u8, f[2].parse().unwrap(), hex(f[3]));
            assert_eq!(bus.config_read(b, r, size) as u64, want, "{line}");
            // The same value through the ports.
            bus.io_write(0xCF8, 4, addr(b, r & !3));
            assert_eq!(bus.io_read(0xCFC + (r & 3) as u16, size) as u64, want, "{line} via ports");
            checked += 1;
        }
        assert!(checked > 100);
    }

    #[test]
    fn config_mechanism_offsets() {
        let mut bus = PciBus::new(0x10000);
        bus.io_write(0xCF8, 4, addr(VGA_BDF, 0));
        assert_eq!(bus.io_read(0xCF8, 4), addr(VGA_BDF, 0));
        assert_eq!(bus.io_read(0xCFC, 4), 0x1111_1234);
        assert_eq!(bus.io_read(0xCFC, 2), 0x1234);
        assert_eq!(bus.io_read(0xCFE, 2), 0x1111);
        assert_eq!(bus.io_read(0xCFD, 1), 0x12);
        assert_eq!(bus.io_read(0xCFF, 1), 0x11);
        // Misaligned accesses are invalid in QEMU and read 0.
        assert_eq!(bus.io_read(0xCFD, 2), 0);
        assert_eq!(bus.io_read(0xCFE, 4), 0);
        bus.io_write(0xCF8, 4, addr(VGA_BDF, 0x3C));
        bus.io_write(0xCFC, 1, 0x0B);
        assert_eq!(bus.config_read(VGA_BDF, 0x3C, 1), 0x0B);
        // Word write at offset 2 of a device-specific dword.
        bus.io_write(0xCF8, 4, addr(ISA_BRIDGE_BDF, 0x60));
        bus.io_write(0xCFE, 2, 0x0B0A);
        assert_eq!(bus.io_read(0xCFC, 4), 0x0B0A_8080);
        assert_eq!(bus.pirq_route(2), Some(0x0A));
        assert_eq!(bus.pirq_route(0), None);
        // Latch writes narrower than 32 bits are ignored.
        bus.io_write(0xCF8, 2, 0);
        assert_eq!(bus.io_read(0xCF8, 4), addr(ISA_BRIDGE_BDF, 0x60));
        // Enable bit clear: data reads all-ones.
        bus.io_write(0xCF8, 4, 0);
        assert_eq!(bus.io_read(0xCFC, 4), 0xFFFF_FFFF);
        assert_eq!(bus.io_read(0xCFC, 1), 0xFF);
    }

    #[test]
    fn absent_functions_read_all_ones() {
        let mut bus = PciBus::new(0x10000);
        for b in [bdf(0, 1, 2), bdf(0, 3, 0), bdf(0, 0x1F, 0), bdf(1, 0, 0)] {
            assert_eq!(bus.config_read(b, 0, 4), 0xFFFF_FFFF);
            assert_eq!(bus.config_read(b, 0, 2), 0xFFFF);
            bus.config_write(b, 0x10, 4, 0);
            bus.io_write(0xCF8, 4, addr(b, 0));
            assert_eq!(bus.io_read(0xCFC, 4), 0xFFFF_FFFF);
        }
    }

    fn size_bar(bus: &mut PciBus, b: u16, r: u8, probe: u32) -> u32 {
        let old = bus.config_read(b, r, 4);
        bus.config_write(b, r, 4, probe);
        let v = bus.config_read(b, r, 4);
        bus.config_write(b, r, 4, old);
        v
    }

    #[test]
    fn bar_sizing() {
        let mut bus = PciBus::new(0x10000);
        assert_eq!(size_bar(&mut bus, IDE_BDF, 0x20, !0), 0xFFFF_FFF1);
        assert_eq!(size_bar(&mut bus, IDE_BDF, 0x10, !0), 0);
        assert_eq!(size_bar(&mut bus, VGA_BDF, 0x10, !0), 0xFF00_0008);
        assert_eq!(size_bar(&mut bus, VGA_BDF, 0x14, !0), 0);
        assert_eq!(size_bar(&mut bus, VGA_BDF, 0x18, !0), 0xFFFF_F000);
        assert_eq!(size_bar(&mut bus, VGA_BDF, 0x30, 0xFFFF_FFFE), 0xFFFF_0000);
        assert_eq!(size_bar(&mut bus, VGA_BDF, 0x30, !0), 0xFFFF_0001);
        assert_eq!(size_bar(&mut bus, HOST_BRIDGE_BDF, 0x30, 0xFFFF_FFFE), 0);
        assert_eq!(size_bar(&mut bus, PM_BDF, 0x10, !0), 0);
        // Header fields are read-only.
        bus.config_write(VGA_BDF, 0, 4, 0);
        assert_eq!(bus.config_read(VGA_BDF, 0, 4), 0x1111_1234);
    }

    #[test]
    fn bar_decode_and_rom_enable() {
        let mut bus = PciBus::new(0x10000);
        bus.config_write(VGA_BDF, 0x10, 4, 0xFD00_0000);
        bus.config_write(VGA_BDF, 0x18, 4, 0xFEBF_0000);
        bus.config_write(VGA_BDF, 0x30, 4, 0xFEBE_0000);
        bus.config_write(IDE_BDF, 0x20, 4, 0xC000);
        assert_eq!(bus.vga_bar(0), None);
        assert_eq!(bus.ide_bmdma_base(), None);
        bus.config_write(VGA_BDF, 4, 2, 0x103);
        bus.config_write(IDE_BDF, 4, 2, 0x103);
        assert_eq!(bus.config_read(VGA_BDF, 4, 2), 0x103);
        assert_eq!(bus.vga_bar(0), Some(0xFD00_0000));
        assert_eq!(bus.vga_bar(2), Some(0xFEBF_0000));
        assert_eq!(bus.vga_bar(1), None);
        assert_eq!(bus.vga_rom_base(), None);
        bus.config_write(VGA_BDF, 0x30, 4, 0xFEBE_0001);
        assert_eq!(bus.vga_rom_base(), Some(0xFEBE_0000));
        assert_eq!(bus.ide_bmdma_base(), Some(0xC000));
        // While sizing, the all-ones pattern is not a mapping.
        bus.config_write(VGA_BDF, 0x10, 4, !0);
        assert_eq!(bus.vga_bar(0), None);
        bus.config_write(IDE_BDF, 0x20, 4, !0);
        assert_eq!(bus.ide_bmdma_base(), None);
    }

    #[test]
    fn status_w1c() {
        let mut bus = PciBus::new(0x10000);
        assert_eq!(bus.config_read(IDE_BDF, 6, 2), 0x0280);
        bus.function_mut(IDE_BDF).unwrap().set_u16(6, 0x2280);
        bus.config_write(IDE_BDF, 6, 2, 0x2000 | 0x0280);
        assert_eq!(bus.config_read(IDE_BDF, 6, 2), 0x0280);
    }

    #[test]
    fn pam_and_smram() {
        let mut bus = PciBus::new(0x10000);
        assert_eq!(bus.pam(), [0; 7]);
        assert_eq!(bus.smram(), 0x02);
        assert_eq!(bus.config_read(HOST_BRIDGE_BDF, 0x57, 1), 0x80);
        bus.config_write(HOST_BRIDGE_BDF, 0x58, 4, 0x3333_3000);
        bus.config_write(HOST_BRIDGE_BDF, 0x5C, 4, 0x3333_3333);
        assert_eq!(bus.pam(), [0x30, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33]);
        bus.config_write(HOST_BRIDGE_BDF, 0x58, 4, 0x1111_1000);
        bus.config_write(HOST_BRIDGE_BDF, 0x5C, 4, 0x3331_1111);
        assert_eq!(bus.pam(), [0x10, 0x11, 0x11, 0x11, 0x11, 0x31, 0x33]);
        bus.config_write(HOST_BRIDGE_BDF, 0x72, 1, 0x4A);
        assert_eq!(bus.smram(), 0x4A);
    }

    #[test]
    fn pm_io_base_decode() {
        let mut bus = PciBus::new(0x10000);
        assert_eq!(bus.config_read(PM_BDF, 0x40, 4), 1);
        assert_eq!(bus.config_read(PM_BDF, 0x5B, 1), 2);
        assert_eq!(bus.pm_io_base(), None);
        bus.config_write(PM_BDF, 0x40, 4, 0x601);
        assert_eq!(bus.pm_io_base(), None);
        let misc = bus.config_read(PM_BDF, 0x80, 1);
        bus.config_write(PM_BDF, 0x80, 1, misc | 1);
        assert_eq!(bus.pm_io_base(), Some(0x600));
        assert_eq!(bus.smbus_io_base(), None);
        bus.config_write(PM_BDF, 0x90, 4, 0x701);
        bus.config_write(PM_BDF, 0xD2, 1, 0x09);
        assert_eq!(bus.smbus_io_base(), Some(0x700));
    }

    #[test]
    fn reset_control_register() {
        let mut bus = PciBus::new(0x10000);
        assert!(!bus.take_reset_request());
        bus.io_write(0xCF9, 1, 0x02);
        assert_eq!(bus.io_read(0xCF9, 1), 0x02);
        assert!(!bus.take_reset_request());
        bus.io_write(0xCF9, 1, 0x06);
        assert!(bus.take_reset_request());
        assert!(!bus.take_reset_request());
        assert_eq!(bus.io_read(0xCF9, 1), 0x02, "a reset write does not latch");
        // A 32-bit latch write at 0xCF8 does not touch RCR.
        bus.io_write(0xCF8, 4, 0x8000_0400);
        assert!(!bus.take_reset_request());
        bus.reset();
        assert_eq!(bus.io_read(0xCF9, 1), 0);
    }

    /// Replays every PCI config access of the QEMU reference boot:
    /// `pci_cfg_write` lines are applied with `config_write` and every
    /// `pci_cfg_read` value is checked; a second bus replays the raw
    /// 0xCF8/0xCFC port accesses.
    #[test]
    #[ignore]
    fn replay_pci_config() {
        let path = std::env::var_os("TEMPLEOS_TRACE")
            .map(|p| workspace_root().join(p))
            .unwrap_or_else(|| workspace_root().join("ref/boot/trace.log"));
        let Ok(text) = std::fs::read(&path) else {
            eprintln!("skipping: trace {} not found", path.display());
            return;
        };
        let text = String::from_utf8_lossy(&text);
        let mut direct = PciBus::new(vga_rom_size());
        let mut ports = PciBus::new(vga_rom_size());
        let (mut reads, mut writes, mut port_ops) = (0, 0, 0);
        let mut mismatches = Vec::new();
        let mut pending_read: Option<(u16, u8, u64, usize)> = None;
        let mut last_data_write_size = 4u8;

        for (n, line) in text.lines().enumerate() {
            let f: Vec<&str> = line.split_whitespace().collect();
            match f.first() {
                Some(&"pci_cfg_read") => {
                    // pci_cfg_read NAME BB:DD.F @0xREG -> 0xVAL
                    let reg = hex(f[3].trim_start_matches('@')) as u8;
                    pending_read = Some((parse_bdf(f[2]), reg, hex(f[5]), n + 1));
                }
                Some(&"pci_cfg_write") => {
                    // pci_cfg_write NAME BB:DD.F @0xREG <- 0xVAL (after the port line)
                    let reg = hex(f[3].trim_start_matches('@')) as u8;
                    direct.config_write(parse_bdf(f[2]), reg, last_data_write_size, hex(f[5]) as u32);
                    writes += 1;
                }
                Some(&op @ ("memory_region_ops_read" | "memory_region_ops_write"))
                    if line.ends_with("'pci-conf-data'") || line.ends_with("'pci-conf-idx'") =>
                {
                    // ... addr 0xPORT value 0xVAL size N name '...'
                    let port = hex(f[6]) as u16;
                    let val = hex(f[8]);
                    let size: u8 = f[10].parse().unwrap();
                    let mask = size_mask(size) as u64;
                    port_ops += 1;
                    if op == "memory_region_ops_write" {
                        ports.io_write(port, size, val as u32);
                        if port >= 0xCFC {
                            last_data_write_size = size;
                        }
                    } else {
                        let got = ports.io_read(port, size) as u64;
                        if got != val & mask {
                            mismatches.push(format!(
                                "line {}: port {port:#x} size {size}: got {got:#x}, want {:#x}",
                                n + 1,
                                val & mask
                            ));
                        }
                        if let Some((b, reg, want, ln)) = pending_read.take() {
                            let got = direct.config_read(b, reg, size) as u64;
                            reads += 1;
                            if got != want & mask {
                                mismatches.push(format!(
                                    "line {ln}: {b:04x} @{reg:#x} size {size}: got {got:#x}, want {:#x}",
                                    want & mask
                                ));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        eprintln!(
            "replayed {reads} config reads, {writes} config writes, {port_ops} port accesses; {} mismatches",
            mismatches.len()
        );
        for m in mismatches.iter().take(50) {
            eprintln!("  {m}");
        }
        assert!(reads > 0, "no pci_cfg_read lines in {}", path.display());
        assert!(mismatches.is_empty());
        assert_eq!(direct.pam(), ports.pam());
    }
}
