//! QEMU's standard VGA (`-device VGA`, PCI 1234:1111): the legacy VGA
//! register file at 0x3B0-0x3DF, the Bochs VBE "DISPI" interface at
//! 0x1CE/0x1CF, the BAR 2 MMIO window (EDID, VGA ports, DISPI, QEMU
//! extended registers), the option ROM and the 16 MiB VRAM.
//!
//! Register semantics follow QEMU 8.2 `hw/display/vga.c` (`vga_ioport_read`,
//! `vga_ioport_write`, `vbe_ioport_*`) and `hw/display/vga-pci.c`.
//! Planar memory access through 0xA0000 and rendering are not modelled here
//! yet (Phase 3); everything they need is in [`VgaRegs`] and [`Vga::vram`].

use crate::unhandled;

/// Size of the video memory (QEMU's default `vgamem_mb=16`).
pub const VRAM_SIZE: usize = 16 << 20;

/// The EDID blob QEMU's std VGA exposes at BAR 2 offset 0 (captured from the
/// reference machine, docs/ref/edid-stdvga.txt).
pub const EDID: [u8; 256] = [
    0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x49, 0x14, 0x34, 0x12, 0x00, 0x00, 0x00, 0x00,
    0x2a, 0x18, 0x01, 0x04, 0xa5, 0x20, 0x14, 0x78, 0x06, 0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26,
    0x0f, 0x50, 0x54, 0x21, 0x08, 0x00, 0xe1, 0xc0, 0xd1, 0xc0, 0xd1, 0x00, 0xa9, 0x40, 0xb3, 0x00,
    0x95, 0x00, 0x81, 0x80, 0x81, 0x40, 0xea, 0x29, 0x00, 0xc0, 0x51, 0x20, 0x1c, 0x30, 0x40, 0x26,
    0x44, 0x40, 0x45, 0xcb, 0x10, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0xf7, 0x00, 0x0a, 0x00, 0x40,
    0x82, 0x00, 0x28, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xfd, 0x00, 0x32,
    0x7d, 0x1e, 0xa0, 0xff, 0x01, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0xfc,
    0x00, 0x51, 0x45, 0x4d, 0x55, 0x20, 0x4d, 0x6f, 0x6e, 0x69, 0x74, 0x6f, 0x72, 0x0a, 0x01, 0x3a,
    0x02, 0x03, 0x0b, 0x00, 0x46, 0x7d, 0x65, 0x60, 0x59, 0x1f, 0x61, 0x00, 0x00, 0x00, 0x10, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2f,
];

/// Bochs VBE DISPI register indexes and values (QEMU `include/hw/display/bochs-vbe.h`).
pub mod dispi {
    pub const INDEX_ID: u16 = 0x0;
    pub const INDEX_XRES: u16 = 0x1;
    pub const INDEX_YRES: u16 = 0x2;
    pub const INDEX_BPP: u16 = 0x3;
    pub const INDEX_ENABLE: u16 = 0x4;
    pub const INDEX_BANK: u16 = 0x5;
    pub const INDEX_VIRT_WIDTH: u16 = 0x6;
    pub const INDEX_VIRT_HEIGHT: u16 = 0x7;
    pub const INDEX_X_OFFSET: u16 = 0x8;
    pub const INDEX_Y_OFFSET: u16 = 0x9;
    /// Number of stored registers (`VBE_DISPI_INDEX_NB`).
    pub const INDEX_NB: u16 = 0xa;
    /// Read-only: VRAM size in 64 KiB units.
    pub const INDEX_VIDEO_MEMORY_64K: u16 = 0xa;

    pub const ID0: u16 = 0xb0c0;
    pub const ID5: u16 = 0xb0c5;

    pub const MAX_XRES: u16 = 16000;
    pub const MAX_YRES: u16 = 12000;
    pub const MAX_BPP: u16 = 32;

    pub const ENABLED: u16 = 0x01;
    pub const GETCAPS: u16 = 0x02;
    pub const DAC_8BIT: u16 = 0x20;
    pub const LFB_ENABLED: u16 = 0x40;
    pub const NOCLEARMEM: u16 = 0x80;
}

/// Writable bits of each sequencer register (QEMU `sr_mask`).
const SR_MASK: [u8; 8] = [0x03, 0x3d, 0x0f, 0x3f, 0x0e, 0x00, 0x00, 0xff];
/// Writable bits of each graphics controller register (QEMU `gr_mask`).
const GR_MASK: [u8; 16] = [
    0x0f, 0x0f, 0x0f, 0x1f, 0x03, 0x7b, 0x0f, 0x0f, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Number of attribute controller registers (`VGA_ATT_C`).
const AR_COUNT: usize = 0x15;

const CR_OVERFLOW: usize = 0x07;
const CR_MAX_SCAN: usize = 0x09;
const CR_V_SYNC_END: usize = 0x11;
const CR_V_DISP_END: usize = 0x12;
const CR_OFFSET: usize = 0x13;
const CR_H_DISP: usize = 0x01;
const CR_MODE: usize = 0x17;
const CR_LINE_COMPARE: usize = 0x18;
const GR_MODE: usize = 0x05;
const GR_MISC: usize = 0x06;
const SR_PLANE_WRITE: usize = 0x02;
const SR_CLOCK_MODE: usize = 0x01;
const SR_MEMORY_MODE: usize = 0x04;

/// Input status 1 bits QEMU's "dumb" retrace toggles on every read.
const ST01_V_RETRACE: u8 = 0x08;
const ST01_DISP_ENABLE: u8 = 0x01;

/// The complete guest-visible register state, in QEMU's layout. Phase 3's
/// planar memory and renderer read it through [`Vga::regs`].
#[derive(Clone, Debug)]
pub struct VgaRegs {
    /// Miscellaneous output (write 0x3C2, read 0x3CC). Bit 0 selects colour
    /// (0x3Dx) vs mono (0x3Bx) CRTC/status ports; bit 4 is never stored.
    pub msr: u8,
    /// Feature control (write 0x3BA/0x3DA, read 0x3CA); only bit 4 is kept.
    pub fcr: u8,
    /// Input status 0 (read 0x3C2). QEMU never sets it.
    pub st00: u8,
    /// Input status 1 (read 0x3BA/0x3DA): bits 0 and 3 toggle on every read.
    pub st01: u8,
    pub sr_index: u8,
    /// Sequencer registers as the guest wrote them (masked by `SR_MASK`).
    pub sr: [u8; 8],
    /// Sequencer registers as the hardware uses them while VBE is enabled
    /// (QEMU `sr_vbe`): chain-4 and all-planes forced for bpp > 4.
    pub sr_vbe: [u8; 8],
    pub gr_index: u8,
    pub gr: [u8; 16],
    pub cr_index: u8,
    pub cr: [u8; 256],
    /// Attribute controller index as last written (6 bits, bit 5 = PAS).
    pub ar_index: u8,
    /// false: next 0x3C0 write is the index; true: it is the data.
    pub ar_flip_flop: bool,
    pub ar: [u8; AR_COUNT],
    /// DAC state (read 0x3C7): 0 after a write-index write, 3 after a read-index write.
    pub dac_state: u8,
    pub dac_sub_index: u8,
    pub dac_read_index: u8,
    pub dac_write_index: u8,
    pub dac_cache: [u8; 3],
    /// Set by the DISPI 8-bit DAC enable flag.
    pub dac_8bit: bool,
    /// 256 RGB triples as written through 0x3C9. Like QEMU, values are stored
    /// unmasked; the renderer uses the low 6 bits unless `dac_8bit`.
    pub palette: [u8; 768],
    pub vbe_index: u16,
    pub vbe_regs: [u16; dispi::INDEX_NB as usize],
    /// Bytes per scanline in VBE modes (from VIRT_WIDTH and BPP).
    pub vbe_line_offset: u32,
    /// Display start in VBE modes, in 4-byte units.
    pub vbe_start_addr: u32,
    /// Byte offset of the banked 0xA0000 window (DISPI BANK << 16).
    pub bank_offset: u32,
    /// Framebuffer byte order selected through the QEMU extended registers.
    pub big_endian_fb: bool,
}

impl Default for VgaRegs {
    /// QEMU's `vga_common_reset` state: everything zero (so mono port range
    /// active) and the DISPI ID set to 0xB0C5.
    fn default() -> Self {
        let mut vbe_regs = [0; dispi::INDEX_NB as usize];
        vbe_regs[dispi::INDEX_ID as usize] = dispi::ID5;
        Self {
            msr: 0,
            fcr: 0,
            st00: 0,
            st01: 0,
            sr_index: 0,
            sr: [0; 8],
            sr_vbe: [0; 8],
            gr_index: 0,
            gr: [0; 16],
            cr_index: 0,
            cr: [0; 256],
            ar_index: 0,
            ar_flip_flop: false,
            ar: [0; AR_COUNT],
            dac_state: 0,
            dac_sub_index: 0,
            dac_read_index: 0,
            dac_write_index: 0,
            dac_cache: [0; 3],
            dac_8bit: false,
            palette: [0; 768],
            vbe_index: 0,
            vbe_regs,
            vbe_line_offset: 0,
            vbe_start_addr: 0,
            bank_offset: 0,
            big_endian_fb: false,
        }
    }
}

impl VgaRegs {
    /// True while a DISPI (VBE) mode is enabled.
    pub fn vbe_enabled(&self) -> bool {
        self.vbe_regs[dispi::INDEX_ENABLE as usize] & dispi::ENABLED != 0
    }

    /// Sequencer register `idx` as the display hardware sees it (QEMU `sr()`).
    pub fn effective_sr(&self, idx: usize) -> u8 {
        if self.vbe_enabled() {
            self.sr_vbe[idx]
        } else {
            self.sr[idx]
        }
    }

    /// True if bit 0 of the misc output register selects the colour ports.
    pub fn color(&self) -> bool {
        self.msr & 0x01 != 0
    }
}

/// The std VGA device.
pub struct Vga {
    regs: VgaRegs,
    vram: Vec<u8>,
    rom: Vec<u8>,
}

const PCI_VGA_QEXT_SIZE: u64 = 8;
const PCI_VGA_QEXT_LITTLE_ENDIAN: u64 = 0x1e1e_1e1e;
const PCI_VGA_QEXT_BIG_ENDIAN: u64 = 0xbebe_bebe;

impl Vga {
    /// A VGA in its reset state with `rom` (the SeaVGABIOS image) as the
    /// PCI option ROM. The ROM is zero-padded to a power of two of at least
    /// 2 KiB, as QEMU sizes the ROM BAR. (QEMU's `pci_patch_ids` is not
    /// applied: SeaVGABIOS's PCIR IDs already match 1234:1111.)
    pub fn new(rom: &[u8]) -> Self {
        let size = rom.len().next_power_of_two().max(2048);
        let mut padded = vec![0; size];
        padded[..rom.len()].copy_from_slice(rom);
        Self {
            regs: VgaRegs::default(),
            vram: vec![0; VRAM_SIZE],
            rom: padded,
        }
    }

    /// The register file, for the renderer and planar memory model.
    pub fn regs(&self) -> &VgaRegs {
        &self.regs
    }

    /// The padded option ROM image.
    pub fn rom(&self) -> &[u8] {
        &self.rom
    }

    pub fn vram(&self) -> &[u8] {
        &self.vram
    }

    pub fn vram_mut(&mut self) -> &mut [u8] {
        &mut self.vram
    }

    /// True for the ports QEMU registers on the ISA bus (0x3B4/5, 0x3BA,
    /// 0x3C0-0x3CF, 0x3D4/5, 0x3DA). The rest of 0x3B0-0x3DF is unassigned.
    fn is_isa_port(port: u16) -> bool {
        matches!(port, 0x3b4 | 0x3b5 | 0x3ba | 0x3c0..=0x3cf | 0x3d4 | 0x3d5 | 0x3da)
    }

    /// Byte read from the legacy VGA ports. Unassigned ports in 0x3B0-0x3DF
    /// read 0xFF, as on QEMU's ISA bus; ports of the inactive (colour or
    /// mono) range also read 0xFF.
    pub fn io_read(&mut self, port: u16) -> u8 {
        if !Self::is_isa_port(port) {
            unhandled("vga io read", port.into(), 1, None);
            return 0xff;
        }
        self.ioport_read(port)
    }

    /// Byte write to the legacy VGA ports. Writes to the inactive
    /// colour/mono range are dropped.
    pub fn io_write(&mut self, port: u16, val: u8) {
        if !Self::is_isa_port(port) {
            unhandled("vga io write", port.into(), 1, Some(val.into()));
            return;
        }
        self.ioport_write(port, val);
    }

    fn ioport_invalid(&self, port: u16) -> bool {
        if self.regs.color() {
            (0x3b0..=0x3bf).contains(&port)
        } else {
            (0x3d0..=0x3df).contains(&port)
        }
    }

    /// QEMU `vga_ioport_read`. Ports without a register read 0.
    fn ioport_read(&mut self, port: u16) -> u8 {
        if self.ioport_invalid(port) {
            return 0xff;
        }
        let r = &mut self.regs;
        match port {
            0x3c0 => {
                if r.ar_flip_flop {
                    0
                } else {
                    r.ar_index
                }
            }
            0x3c1 => {
                let index = (r.ar_index & 0x1f) as usize;
                if index < AR_COUNT {
                    r.ar[index]
                } else {
                    0
                }
            }
            0x3c2 => r.st00,
            0x3c4 => r.sr_index,
            0x3c5 => r.sr[r.sr_index as usize],
            0x3c7 => r.dac_state,
            0x3c8 => r.dac_write_index,
            0x3c9 => {
                let val = r.palette[r.dac_read_index as usize * 3 + r.dac_sub_index as usize];
                r.dac_sub_index += 1;
                if r.dac_sub_index == 3 {
                    r.dac_sub_index = 0;
                    r.dac_read_index = r.dac_read_index.wrapping_add(1);
                }
                val
            }
            0x3ca => r.fcr,
            0x3cc => r.msr,
            0x3ce => r.gr_index,
            0x3cf => r.gr[r.gr_index as usize],
            0x3b4 | 0x3d4 => r.cr_index,
            0x3b5 | 0x3d5 => r.cr[r.cr_index as usize],
            0x3ba | 0x3da => {
                // QEMU's default "dumb" retrace: toggle to fool polling loops.
                r.st01 ^= ST01_V_RETRACE | ST01_DISP_ENABLE;
                r.ar_flip_flop = false;
                r.st01
            }
            // 0x3C3, 0x3C6 (QEMU has no DAC pixel mask), 0x3CB, 0x3CD, and
            // the unassigned ports reachable only through BAR 2.
            _ => 0,
        }
    }

    /// QEMU `vga_ioport_write`.
    fn ioport_write(&mut self, port: u16, val: u8) {
        if self.ioport_invalid(port) {
            return;
        }
        let r = &mut self.regs;
        match port {
            0x3c0 => {
                if !r.ar_flip_flop {
                    r.ar_index = val & 0x3f;
                } else {
                    let index = (r.ar_index & 0x1f) as usize;
                    match index {
                        0x00..=0x0f => r.ar[index] = val & 0x3f,
                        0x10 => r.ar[index] = val & !0x10,
                        0x11 => r.ar[index] = val,
                        0x12 => r.ar[index] = val & !0xc0,
                        0x13 | 0x14 => r.ar[index] = val & !0xf0,
                        _ => {}
                    }
                }
                r.ar_flip_flop = !r.ar_flip_flop;
            }
            0x3c2 => r.msr = val & !0x10,
            0x3c4 => r.sr_index = val & 7,
            0x3c5 => {
                let i = r.sr_index as usize;
                r.sr[i] = val & SR_MASK[i];
                r.sr_vbe[i] = r.sr[i];
                self.vbe_update_vgaregs();
            }
            0x3c7 => {
                r.dac_read_index = val;
                r.dac_sub_index = 0;
                r.dac_state = 3;
            }
            0x3c8 => {
                r.dac_write_index = val;
                r.dac_sub_index = 0;
                r.dac_state = 0;
            }
            0x3c9 => {
                r.dac_cache[r.dac_sub_index as usize] = val;
                r.dac_sub_index += 1;
                if r.dac_sub_index == 3 {
                    let at = r.dac_write_index as usize * 3;
                    r.palette[at..at + 3].copy_from_slice(&r.dac_cache);
                    r.dac_sub_index = 0;
                    r.dac_write_index = r.dac_write_index.wrapping_add(1);
                }
            }
            0x3ce => r.gr_index = val & 0x0f,
            0x3cf => {
                let i = r.gr_index as usize;
                r.gr[i] = val & GR_MASK[i];
                self.vbe_update_vgaregs();
            }
            0x3b4 | 0x3d4 => r.cr_index = val,
            0x3b5 | 0x3d5 => {
                let i = r.cr_index as usize;
                // CR11 bit 7 write-protects CR0-CR7, except CR7 bit 4.
                if r.cr[CR_V_SYNC_END] & 0x80 != 0 && i <= CR_OVERFLOW {
                    if i == CR_OVERFLOW {
                        r.cr[CR_OVERFLOW] = (r.cr[CR_OVERFLOW] & !0x10) | (val & 0x10);
                        self.vbe_update_vgaregs();
                    }
                    return;
                }
                r.cr[i] = val;
                self.vbe_update_vgaregs();
            }
            0x3ba | 0x3da => r.fcr = val & 0x10,
            _ => {}
        }
    }

    /// Read from the Bochs VBE ports: 0x1CE index, 0x1CF data (0x1D0 is a
    /// second data port on x86, as in QEMU).
    pub fn dispi_read(&mut self, port: u16) -> u16 {
        match port {
            0x1ce => self.regs.vbe_index,
            0x1cf | 0x1d0 => self.vbe_read_data(),
            _ => {
                unhandled("dispi read", port.into(), 2, None);
                0xffff
            }
        }
    }

    /// Write to the Bochs VBE ports.
    pub fn dispi_write(&mut self, port: u16, val: u16) {
        match port {
            0x1ce => self.regs.vbe_index = val,
            0x1cf | 0x1d0 => self.vbe_write_data(val),
            _ => unhandled("dispi write", port.into(), 2, Some(val.into())),
        }
    }

    /// QEMU `vbe_ioport_read_data`.
    fn vbe_read_data(&self) -> u16 {
        let r = &self.regs;
        let index = r.vbe_index;
        if index < dispi::INDEX_NB {
            if r.vbe_regs[dispi::INDEX_ENABLE as usize] & dispi::GETCAPS != 0 {
                match index {
                    dispi::INDEX_XRES => return dispi::MAX_XRES,
                    dispi::INDEX_YRES => return dispi::MAX_YRES,
                    dispi::INDEX_BPP => return dispi::MAX_BPP,
                    _ => {}
                }
            }
            r.vbe_regs[index as usize]
        } else if index == dispi::INDEX_VIDEO_MEMORY_64K {
            (VRAM_SIZE >> 16) as u16
        } else {
            0
        }
    }

    /// QEMU `vbe_ioport_write_data`.
    fn vbe_write_data(&mut self, val: u16) {
        let index = self.regs.vbe_index;
        if index > dispi::INDEX_NB {
            return;
        }
        let regs = &mut self.regs.vbe_regs;
        match index {
            dispi::INDEX_ID => {
                if (dispi::ID0..=dispi::ID5).contains(&val) {
                    regs[index as usize] = val;
                }
            }
            dispi::INDEX_XRES
            | dispi::INDEX_YRES
            | dispi::INDEX_BPP
            | dispi::INDEX_VIRT_WIDTH
            | dispi::INDEX_X_OFFSET
            | dispi::INDEX_Y_OFFSET => {
                regs[index as usize] = val;
                self.vbe_fixup_regs();
                self.vbe_update_vgaregs();
            }
            dispi::INDEX_BANK => {
                let bank_mask = ((VRAM_SIZE >> 16) - 1) as u16;
                let val = val & bank_mask;
                regs[index as usize] = val;
                self.regs.bank_offset = u32::from(val) << 16;
            }
            dispi::INDEX_ENABLE => {
                let was_enabled = regs[index as usize] & dispi::ENABLED != 0;
                if val & dispi::ENABLED != 0 && !was_enabled {
                    regs[dispi::INDEX_VIRT_WIDTH as usize] = 0;
                    regs[dispi::INDEX_X_OFFSET as usize] = 0;
                    regs[dispi::INDEX_Y_OFFSET as usize] = 0;
                    regs[index as usize] |= dispi::ENABLED;
                    self.vbe_fixup_regs();
                    self.vbe_update_vgaregs();
                    if val & dispi::NOCLEARMEM == 0 {
                        let len = usize::from(self.regs.vbe_regs[dispi::INDEX_YRES as usize])
                            * self.regs.vbe_line_offset as usize;
                        self.vram[..len.min(VRAM_SIZE)].fill(0);
                    }
                } else {
                    self.regs.bank_offset = 0;
                }
                self.regs.dac_8bit = val & dispi::DAC_8BIT != 0;
                self.regs.vbe_regs[index as usize] = val;
            }
            _ => {}
        }
    }

    /// QEMU `vbe_fixup_regs`: clamp the mode registers to what VRAM holds and
    /// derive VIRT_HEIGHT, the line length and the start address.
    fn vbe_fixup_regs(&mut self) {
        if !self.regs.vbe_enabled() {
            return;
        }
        let r = &mut self.regs.vbe_regs;
        let bits: u32 = match r[dispi::INDEX_BPP as usize] {
            b @ (4 | 8 | 16 | 24 | 32) => b.into(),
            15 => 16,
            _ => {
                r[dispi::INDEX_BPP as usize] = 8;
                8
            }
        };

        let xres = &mut r[dispi::INDEX_XRES as usize];
        *xres &= !7;
        if *xres == 0 {
            *xres = 8;
        }
        *xres = (*xres).min(dispi::MAX_XRES);
        let xres = *xres;
        let vw = &mut r[dispi::INDEX_VIRT_WIDTH as usize];
        *vw &= !7;
        *vw = (*vw).min(dispi::MAX_XRES).max(xres);

        let vbe_size = VRAM_SIZE as u32;
        let linelength = u32::from(r[dispi::INDEX_VIRT_WIDTH as usize]) * bits / 8;
        let maxy = vbe_size / linelength;
        let yres = &mut r[dispi::INDEX_YRES as usize];
        if *yres == 0 {
            *yres = 1;
        }
        *yres = (*yres).min(dispi::MAX_YRES);
        if u32::from(*yres) > maxy {
            *yres = maxy as u16;
        }
        let yres = u32::from(*yres);

        r[dispi::INDEX_X_OFFSET as usize] = r[dispi::INDEX_X_OFFSET as usize].min(dispi::MAX_XRES);
        r[dispi::INDEX_Y_OFFSET as usize] = r[dispi::INDEX_Y_OFFSET as usize].min(dispi::MAX_YRES);
        let mut offset = u32::from(r[dispi::INDEX_X_OFFSET as usize]) * bits / 8
            + u32::from(r[dispi::INDEX_Y_OFFSET as usize]) * linelength;
        if offset + yres * linelength > vbe_size {
            r[dispi::INDEX_Y_OFFSET as usize] = 0;
            offset = u32::from(r[dispi::INDEX_X_OFFSET as usize]) * bits / 8;
            if offset + yres * linelength > vbe_size {
                r[dispi::INDEX_X_OFFSET as usize] = 0;
                offset = 0;
            }
        }

        // VIRT_HEIGHT is a 16-bit register; QEMU truncates the same way.
        r[dispi::INDEX_VIRT_HEIGHT as usize] = maxy as u16;
        self.regs.vbe_line_offset = linelength;
        self.regs.vbe_start_addr = offset / 4;
    }

    /// QEMU `vbe_update_vgaregs`: while VBE is enabled, force the VGA
    /// registers into a graphics mode matching the DISPI geometry. These
    /// forced values are what the guest reads back.
    fn vbe_update_vgaregs(&mut self) {
        if !self.regs.vbe_enabled() {
            return;
        }
        let r = &mut self.regs;
        r.gr[GR_MISC] = (r.gr[GR_MISC] & !0x0c) | 0x04 | 0x01;
        r.cr[CR_MODE] |= 3;
        r.cr[CR_OFFSET] = (r.vbe_line_offset >> 3) as u8;
        r.cr[CR_H_DISP] = ((r.vbe_regs[dispi::INDEX_XRES as usize] >> 3) as u8).wrapping_sub(1);
        let h = r.vbe_regs[dispi::INDEX_YRES as usize].wrapping_sub(1);
        r.cr[CR_V_DISP_END] = h as u8;
        r.cr[CR_OVERFLOW] =
            (r.cr[CR_OVERFLOW] & !0x42) | ((h >> 7) & 0x02) as u8 | ((h >> 3) & 0x40) as u8;
        r.cr[CR_LINE_COMPARE] = 0xff;
        r.cr[CR_OVERFLOW] |= 0x10;
        r.cr[CR_MAX_SCAN] |= 0x40;
        let shift_control = if r.vbe_regs[dispi::INDEX_BPP as usize] == 4 {
            r.sr_vbe[SR_CLOCK_MODE] &= !8;
            0
        } else {
            r.sr_vbe[SR_MEMORY_MODE] |= 0x08;
            r.sr_vbe[SR_PLANE_WRITE] |= 0x0f;
            2
        };
        r.gr[GR_MODE] = (r.gr[GR_MODE] & !0x60) | (shift_control << 5);
        r.cr[CR_MAX_SCAN] &= !0x9f;
    }

    /// Read from the 4 KiB BAR 2 MMIO window. Accesses QEMU would reject
    /// (wrong size, misaligned, or outside a subregion) read 0.
    ///
    /// - 0x000-0x0FF: EDID (0x100-0x3FF read 0), byte-wise, 1-4 bytes.
    /// - 0x400-0x41F: VGA ports 0x3C0-0x3DF, byte-wise little endian, 1-4 bytes.
    /// - 0x500-0x515: DISPI register `(off - 0x500) / 2`, accessed as 16-bit
    ///   units; each access also sets the DISPI index, as in QEMU.
    /// - 0x600-0x607: QEMU extended registers, 32-bit only.
    pub fn bar2_read(&mut self, offset: u32, size: u8) -> u64 {
        let off = u64::from(offset);
        match offset {
            0x000..=0x3ff if Self::valid(off, size, 1, 4) => (0..u64::from(size))
                .map(|i| u64::from(EDID.get((off + i) as usize).copied().unwrap_or(0)) << (i * 8))
                .fold(0, |a, b| a | b),
            0x400..=0x41f if Self::valid(off, size, 1, 4) => (0..u64::from(size))
                .map(|i| u64::from(self.ioport_read((0x3c0 + off - 0x400 + i) as u16)) << (i * 8))
                .fold(0, |a, b| a | b),
            0x500..=0x515 if Self::valid(off, size, 1, 4) => {
                // impl size 2: a byte access reads the whole register at
                // `off >> 1` and keeps the low byte, as QEMU's
                // access_with_adjusted_size does.
                let mut v = 0u64;
                for i in (0..u64::from(size)).step_by(2) {
                    self.regs.vbe_index = ((off - 0x500 + i) >> 1) as u16;
                    v |= u64::from(self.vbe_read_data()) << (i * 8);
                }
                v & Self::size_mask(size)
            }
            0x600..=0x607 if Self::valid(off, size, 4, 4) => match offset - 0x600 {
                0 => PCI_VGA_QEXT_SIZE,
                4 => {
                    if self.regs.big_endian_fb {
                        PCI_VGA_QEXT_BIG_ENDIAN
                    } else {
                        PCI_VGA_QEXT_LITTLE_ENDIAN
                    }
                }
                _ => 0,
            },
            _ => {
                unhandled("vga bar2 read", off, size, None);
                0
            }
        }
    }

    /// Write to the BAR 2 MMIO window; see [`Vga::bar2_read`] for the layout.
    /// EDID writes are ignored.
    pub fn bar2_write(&mut self, offset: u32, size: u8, val: u64) {
        let off = u64::from(offset);
        match offset {
            0x000..=0x3ff if Self::valid(off, size, 1, 4) => {}
            0x400..=0x41f if Self::valid(off, size, 1, 4) => {
                // Low byte first, so an index/data pair can be set with one
                // 16-bit write.
                for i in 0..u64::from(size) {
                    self.ioport_write((0x3c0 + off - 0x400 + i) as u16, (val >> (i * 8)) as u8);
                }
            }
            0x500..=0x515 if Self::valid(off, size, 1, 4) => {
                let val = val & Self::size_mask(size);
                for i in (0..u64::from(size)).step_by(2) {
                    self.regs.vbe_index = ((off - 0x500 + i) >> 1) as u16;
                    self.vbe_write_data((val >> (i * 8)) as u16);
                }
            }
            0x600..=0x607 if Self::valid(off, size, 4, 4) => {
                if offset == 0x604 {
                    match val & 0xffff_ffff {
                        PCI_VGA_QEXT_BIG_ENDIAN => self.regs.big_endian_fb = true,
                        PCI_VGA_QEXT_LITTLE_ENDIAN => self.regs.big_endian_fb = false,
                        _ => {}
                    }
                }
            }
            _ => unhandled("vga bar2 write", off, size, Some(val)),
        }
    }

    /// QEMU `memory_region_access_valid`: size within the region's limits,
    /// a power of two, and naturally aligned.
    fn valid(off: u64, size: u8, min: u8, max: u8) -> bool {
        (min..=max).contains(&size) && size.is_power_of_two() && off.is_multiple_of(u64::from(size))
    }

    fn size_mask(size: u8) -> u64 {
        if size >= 8 {
            u64::MAX
        } else {
            (1u64 << (size * 8)) - 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vga() -> Vga {
        Vga::new(&[0x55, 0xaa, 0x01])
    }

    /// 16-bit OUT as the board delivers it: low byte to port, high to port+1.
    fn outw(v: &mut Vga, port: u16, val: u16) {
        v.io_write(port, val as u8);
        v.io_write(port + 1, (val >> 8) as u8);
    }

    #[test]
    fn reset_state_is_mono() {
        let mut v = vga();
        assert_eq!(v.io_read(0x3cc), 0);
        assert_eq!(v.io_read(0x3d4), 0xff);
        assert_eq!(v.io_read(0x3da), 0xff);
        assert_eq!(v.io_read(0x3b4), 0);
        // Unassigned ports in the range read 0xFF.
        assert_eq!(v.io_read(0x3b0), 0xff);
    }

    #[test]
    fn seavgabios_style_programming() {
        let mut v = vga();
        v.io_write(0x3c2, 0xc3);
        assert_eq!(v.io_read(0x3cc), 0xc3);
        // Misc output never stores bit 4.
        v.io_write(0x3c2, 0xff);
        assert_eq!(v.io_read(0x3cc), 0xef);
        v.io_write(0x3c2, 0xe3);

        // Sequencer: index + data in one OUTW, masked by sr_mask.
        outw(&mut v, 0x3c4, 0x0204);
        outw(&mut v, 0x3c4, 0xff02);
        outw(&mut v, 0x3c4, 0xff01);
        v.io_write(0x3c4, 0x04);
        assert_eq!(v.io_read(0x3c5), 0x02);
        v.io_write(0x3c4, 0x02);
        assert_eq!(v.io_read(0x3c5), 0x0f);
        v.io_write(0x3c4, 0x01);
        assert_eq!(v.io_read(0x3c5), 0x3d);
        v.io_write(0x3c4, 0x0a);
        assert_eq!(v.io_read(0x3c4), 0x02, "sequencer index is 3 bits");

        // Graphics controller.
        outw(&mut v, 0x3ce, 0x1005);
        outw(&mut v, 0x3ce, 0xff08);
        outw(&mut v, 0x3ce, 0xff06);
        v.io_write(0x3ce, 0x05);
        assert_eq!(v.io_read(0x3cf), 0x10);
        v.io_write(0x3ce, 0x08);
        assert_eq!(v.io_read(0x3cf), 0xff);
        v.io_write(0x3ce, 0x06);
        assert_eq!(v.io_read(0x3cf), 0x0f);

        // CRTC through the colour ports; mono ports are dead.
        outw(&mut v, 0x3d4, 0x0011);
        outw(&mut v, 0x3d4, 0x5f00);
        outw(&mut v, 0x3d4, 0x1f07);
        v.io_write(0x3d4, 0x00);
        assert_eq!(v.io_read(0x3d5), 0x5f);
        assert_eq!(v.io_read(0x3b5), 0xff);
        v.io_write(0x3b4, 0x07);
        assert_eq!(v.io_read(0x3d4), 0x00, "mono index write ignored");

        // CR11 bit 7 protects CR0-7 except CR7 bit 4.
        outw(&mut v, 0x3d4, 0x8e11);
        outw(&mut v, 0x3d4, 0x1100);
        outw(&mut v, 0x3d4, 0x0007);
        v.io_write(0x3d4, 0x00);
        assert_eq!(v.io_read(0x3d5), 0x5f);
        v.io_write(0x3d4, 0x07);
        assert_eq!(v.io_read(0x3d5), 0x0f);

        // Back to mono: the 0x3Bx CRTC ports now work.
        v.io_write(0x3c2, 0x62);
        v.io_write(0x3b4, 0x0a);
        v.io_write(0x3b5, 0x0d);
        assert_eq!(v.io_read(0x3b5), 0x0d);
        assert_eq!(v.io_read(0x3d5), 0xff);
        assert_eq!(v.regs().cr[0x0a], 0x0d);
    }

    #[test]
    fn attribute_flip_flop() {
        let mut v = vga();
        v.io_write(0x3c2, 0x01);
        v.io_read(0x3da);
        // Index write, then data write, per register (TempleOS palette setup).
        for i in 0..16u8 {
            v.io_write(0x3c0, i);
            assert_eq!(v.io_read(0x3c0), 0, "reads 0 while the flip-flop expects data");
            v.io_write(0x3c0, i | 0xc0);
            assert_eq!(v.io_read(0x3c0), i);
        }
        v.io_write(0x3c0, 0x10);
        v.io_write(0x3c0, 0xff);
        v.io_write(0x3c0, 0x20); // PAS, index only
        assert_eq!(v.regs().ar_index, 0x20);
        assert_eq!(v.io_read(0x3c1), v.regs().ar[0]);
        for i in 0..16 {
            assert_eq!(v.regs().ar[i], i as u8);
        }
        assert_eq!(v.regs().ar[0x10], 0xef);

        // 0x3DA read resets the flip-flop mid-pair.
        v.io_read(0x3da);
        v.io_write(0x3c0, 0x12);
        v.io_read(0x3da);
        v.io_write(0x3c0, 0x13);
        v.io_write(0x3c0, 0xff);
        assert_eq!(v.regs().ar[0x13], 0x0f);
        assert_eq!(v.regs().ar[0x12], 0);
        v.io_write(0x3c0, 0x13);
        assert_eq!(v.io_read(0x3c1), 0x0f);
    }

    #[test]
    fn input_status_toggles() {
        let mut v = vga();
        v.io_write(0x3c2, 0x01);
        assert_eq!(v.io_read(0x3da), 0x09);
        assert_eq!(v.io_read(0x3da), 0x00);
        assert_eq!(v.io_read(0x3da), 0x09);
        assert_eq!(v.io_read(0x3ba), 0xff);
    }

    #[test]
    fn dac_write_then_read() {
        let mut v = vga();
        v.io_write(0x3c6, 0xff);
        v.io_write(0x3c8, 0x05);
        for c in [0x2a, 0x15, 0x3f, 0x01, 0x02, 0x03] {
            v.io_write(0x3c9, c);
        }
        assert_eq!(v.io_read(0x3c8), 0x07);
        assert_eq!(v.io_read(0x3c7), 0);
        v.io_write(0x3c7, 0x05);
        assert_eq!(v.io_read(0x3c7), 3);
        let got: Vec<u8> = (0..6).map(|_| v.io_read(0x3c9)).collect();
        assert_eq!(got, [0x2a, 0x15, 0x3f, 0x01, 0x02, 0x03]);
        assert_eq!(&v.regs().palette[15..21], &[0x2a, 0x15, 0x3f, 0x01, 0x02, 0x03]);
        // A partial triple isn't committed until the third byte.
        v.io_write(0x3c8, 0x00);
        v.io_write(0x3c9, 0x11);
        assert_eq!(v.regs().palette[0], 0);
        // Index wraps at 256.
        v.io_write(0x3c8, 0xff);
        for _ in 0..3 {
            v.io_write(0x3c9, 0x3f);
        }
        assert_eq!(v.io_read(0x3c8), 0x00);
    }

    #[test]
    fn dispi_id_caps_and_memory() {
        let mut v = vga();
        v.dispi_write(0x1ce, dispi::INDEX_ID);
        assert_eq!(v.dispi_read(0x1cf), 0xb0c5);
        v.dispi_write(0x1cf, 0xb0c0);
        assert_eq!(v.dispi_read(0x1cf), 0xb0c0);
        v.dispi_write(0x1cf, 0x1234);
        assert_eq!(v.dispi_read(0x1cf), 0xb0c0);
        v.dispi_write(0x1cf, 0xb0c5);
        assert_eq!(v.dispi_read(0x1cf), 0xb0c5);

        v.dispi_write(0x1ce, dispi::INDEX_VIDEO_MEMORY_64K);
        assert_eq!(v.dispi_read(0x1cf), 0x100);
        assert_eq!(v.dispi_read(0x1ce), 0xa);

        v.dispi_write(0x1ce, dispi::INDEX_ENABLE);
        assert_eq!(v.dispi_read(0x1cf), 0);
        v.dispi_write(0x1cf, dispi::GETCAPS);
        v.dispi_write(0x1ce, dispi::INDEX_XRES);
        assert_eq!(v.dispi_read(0x1cf), 16000);
        v.dispi_write(0x1ce, dispi::INDEX_YRES);
        assert_eq!(v.dispi_read(0x1cf), 12000);
        v.dispi_write(0x1ce, dispi::INDEX_BPP);
        assert_eq!(v.dispi_read(0x1cf), 32);
        v.dispi_write(0x1ce, dispi::INDEX_ENABLE);
        v.dispi_write(0x1cf, 0);
        v.dispi_write(0x1ce, dispi::INDEX_XRES);
        assert_eq!(v.dispi_read(0x1cf), 0);

        v.dispi_write(0x1ce, 0x0b);
        assert_eq!(v.dispi_read(0x1cf), 0);
    }

    #[test]
    fn dispi_mode_set() {
        let mut v = vga();
        v.vram_mut()[0] = 0xaa;
        v.vram_mut()[640 * 4 * 480] = 0xbb;
        let set = |v: &mut Vga, i: u16, val: u16| {
            v.dispi_write(0x1ce, i);
            v.dispi_write(0x1cf, val);
        };
        let get = |v: &mut Vga, i: u16| {
            v.dispi_write(0x1ce, i);
            v.dispi_read(0x1cf)
        };
        set(&mut v, dispi::INDEX_XRES, 643);
        set(&mut v, dispi::INDEX_YRES, 480);
        set(&mut v, dispi::INDEX_BPP, 32);
        set(&mut v, dispi::INDEX_VIRT_HEIGHT, 5);
        assert_eq!(get(&mut v, dispi::INDEX_XRES), 643, "no fixup while disabled");
        assert_eq!(get(&mut v, dispi::INDEX_VIRT_HEIGHT), 0, "VIRT_HEIGHT is read-only");
        set(&mut v, dispi::INDEX_ENABLE, dispi::ENABLED | dispi::LFB_ENABLED);
        assert_eq!(get(&mut v, dispi::INDEX_XRES), 640);
        assert_eq!(get(&mut v, dispi::INDEX_VIRT_WIDTH), 640);
        assert_eq!(get(&mut v, dispi::INDEX_VIRT_HEIGHT), (VRAM_SIZE / (640 * 4)) as u16);
        assert_eq!(v.regs().vbe_line_offset, 2560);
        assert_eq!(v.vram()[0], 0, "enable clears VRAM");
        assert_eq!(v.vram()[640 * 4 * 480], 0xbb, "only yres * line_offset");
        // Forced VGA registers.
        v.io_write(0x3ce, 0x06);
        assert_eq!(v.io_read(0x3cf) & 0x0d, 0x05);
        assert_eq!(v.regs().cr[0x01], 79);
        assert_eq!(v.regs().cr[0x12], (479 & 0xff) as u8);
        assert_eq!(v.regs().effective_sr(4) & 0x08, 0x08);

        set(&mut v, dispi::INDEX_BPP, 7);
        assert_eq!(get(&mut v, dispi::INDEX_BPP), 8);
        set(&mut v, dispi::INDEX_BANK, 0x1ff);
        assert_eq!(get(&mut v, dispi::INDEX_BANK), 0xff);
        assert_eq!(v.regs().bank_offset, 0xff << 16);

        set(&mut v, dispi::INDEX_ENABLE, 0);
        v.vram_mut()[0] = 0xcc;
        set(&mut v, dispi::INDEX_ENABLE, dispi::ENABLED | dispi::NOCLEARMEM | dispi::DAC_8BIT);
        assert_eq!(v.vram()[0], 0xcc);
        assert!(v.regs().dac_8bit);
    }

    #[test]
    fn bar2_edid_matches_fixture() {
        let text = include_str!("../../docs/ref/edid-stdvga.txt");
        let fixture: Vec<u8> = text
            .lines()
            .filter(|l| !l.starts_with('#'))
            .flat_map(|l| l.split_whitespace())
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect();
        assert_eq!(fixture.len(), 256);
        let mut v = vga();
        for (i, &b) in fixture.iter().enumerate() {
            assert_eq!(v.bar2_read(i as u32, 1), u64::from(b), "EDID byte {i:#x}");
        }
        assert_eq!(v.bar2_read(0, 4), 0xffff_ff00);
        assert_eq!(v.bar2_read(0x100, 1), 0);
        assert_eq!(v.bar2_read(0x3fc, 4), 0);
        v.bar2_write(0, 1, 0x55);
        assert_eq!(v.bar2_read(0, 1), 0);
    }

    #[test]
    fn bar2_vga_port_alias() {
        let mut v = vga();
        v.bar2_write(0x402, 1, 0x01); // 0x3C2 misc output
        assert_eq!(v.io_read(0x3cc), 0x01);
        v.bar2_write(0x414, 2, 0x5f00); // CRTC index 0, data 0x5f
        assert_eq!(v.regs().cr[0], 0x5f);
        assert_eq!(v.bar2_read(0x414, 2), 0x5f00);
        v.bar2_write(0x404, 2, 0x0f02);
        assert_eq!(v.bar2_read(0x405, 1), 0x0f);
        // Toggling status through MMIO too.
        assert_eq!(v.bar2_read(0x41a, 1), 0x09);
        // Unregistered ports read 0 through MMIO (vga_ioport_read default).
        assert_eq!(v.bar2_read(0x403, 1), 0);
    }

    #[test]
    fn bar2_dispi_alias_and_qext() {
        let mut v = vga();
        assert_eq!(v.bar2_read(0x500, 2), 0xb0c5);
        v.bar2_write(0x502, 2, 1024);
        assert_eq!(v.dispi_read(0x1ce), 1, "MMIO access sets the index");
        assert_eq!(v.dispi_read(0x1cf), 1024);
        assert_eq!(v.bar2_read(0x500, 4), 0xb0c5 | (1024 << 16));
        assert_eq!(v.bar2_read(0x514, 2), 0x100);
        v.dispi_write(0x1ce, dispi::INDEX_ENABLE);
        v.dispi_write(0x1cf, dispi::GETCAPS);
        assert_eq!(v.bar2_read(0x502, 2), 16000);

        assert_eq!(v.bar2_read(0x600, 4), 8);
        assert_eq!(v.bar2_read(0x604, 4), 0x1e1e_1e1e);
        v.bar2_write(0x604, 4, 0xbebe_bebe);
        assert_eq!(v.bar2_read(0x604, 4), 0xbebe_bebe);
        assert!(v.regs().big_endian_fb);
        assert_eq!(v.bar2_read(0x604, 2), 0, "qext is 32-bit only");
        assert_eq!(v.bar2_read(0x700, 4), 0);
    }

    #[test]
    fn rom_is_padded() {
        let v = Vga::new(&[0x55, 0xaa, 0x01]);
        assert_eq!(v.rom().len(), 2048);
        assert_eq!(&v.rom()[..3], &[0x55, 0xaa, 0x01]);
        assert!(v.rom()[3..].iter().all(|&b| b == 0));
        let v = Vga::new(&vec![1; 39424]);
        assert_eq!(v.rom().len(), 65536);
        assert_eq!(v.vram().len(), VRAM_SIZE);
    }
}
