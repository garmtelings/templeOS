//! PIIX4 ACPI power-management I/O (QEMU `hw/acpi/piix4.c` + `hw/acpi/core.c`)
//! and the APM control/status ports 0xB2/0xB3 (`hw/isa/apm.c`).
//!
//! The 64-byte PM block sits wherever the guest programs PMBA (SeaBIOS uses
//! 0x600); the board routes accesses there via `PciBus::pm_io_base`. Inside
//! it QEMU implements only three registers, each as its own memory region
//! with its own access rules:
//!
//! | Offset | Register | QEMU access rules |
//! |--------|----------|-------------------|
//! | 0x00   | PM1a_STS (16 bit) | 1-2 byte, aligned; offset 0 = status, 2 = enable, 1/3 read 0 |
//! | 0x02   | PM1a_EN (16 bit)  | |
//! | 0x04   | PM1a_CNT (16 bit) | 1-2 byte, aligned; a byte at 0x05 is the high byte |
//! | 0x08   | PM_TMR (24 bit)   | 1-4 byte, aligned; every read returns the whole counter truncated to the access size |
//!
//! Accesses a region rejects (too wide or misaligned) read 0 and drop writes.
//! The rest of the block is unassigned in QEMU and reads all-ones (the
//! reference trace shows SeaBIOS reading 0x628 as 0xFFFFFFFF with SMM on).
//!
//! The PIIX4 GPE block (0xAFE0) and the PCI/CPU hotplug ports QEMU also
//! registers are not modelled; nothing in SeaBIOS or TempleOS touches them.

use crate::Nanos;

/// ACPI PM timer frequency.
pub const PM_TIMER_HZ: u64 = 3_579_545;

/// PM1 status/enable bits.
pub mod pm1 {
    pub const TMR: u16 = 0x0001;
    pub const GBL: u16 = 0x0020;
    pub const PWRBTN: u16 = 0x0100;
    pub const RTC: u16 = 0x0400;
}

/// PM1 control bits.
pub mod pm1_cnt {
    pub const SCI_EN: u16 = 0x0001;
    pub const SLP_EN: u16 = 0x2000;
}

/// APM control commands that toggle SCI_EN (ACPI 3.0, 4.7.2.5).
pub const ACPI_ENABLE: u8 = 0xF1;
pub const ACPI_DISABLE: u8 = 0xF0;

pub const APM_CNT_PORT: u16 = 0xB2;
pub const APM_STS_PORT: u16 = 0xB3;

/// PIIX4 power management I/O (the 64-byte block at `pm_io_base`), APM
/// ports, SCI.
///
/// The PM timer counts `now * 3.579545 MHz` from machine creation, like
/// QEMU's virtual-clock based timer, and wraps at 24 bits. PM1_STS.TMR_STS
/// is set whenever the counter has passed bit 23 since it was last cleared
/// (QEMU `overflow_time`); after reset it reads as set, as in QEMU.
pub struct Piix4Pm {
    pm1_sts: u16,
    pm1_en: u16,
    pm1_cnt: u16,
    /// Timer tick at which TMR_STS next becomes set.
    overflow_tick: u64,
    apmc: u8,
    apms: u8,
}

impl Default for Piix4Pm {
    fn default() -> Self {
        Self::new()
    }
}

impl Piix4Pm {
    pub fn new() -> Self {
        Piix4Pm { pm1_sts: 0, pm1_en: 0, pm1_cnt: 0, overflow_tick: 0, apmc: 0, apms: 0 }
    }

    /// The full-resolution PM timer tick count at `now`.
    fn ticks(now: Nanos) -> u64 {
        (now as u128 * PM_TIMER_HZ as u128 / 1_000_000_000) as u64
    }

    /// PM_TMR value at `now`: 24 bits at 3.579545 MHz.
    pub fn timer(now: Nanos) -> u32 {
        (Self::ticks(now) & 0xFF_FFFF) as u32
    }

    /// QEMU `acpi_pm1_evt_get_sts`: latch TMR_STS once the overflow point
    /// has passed.
    fn pm1_sts(&mut self, now: Nanos) -> u16 {
        let overflow_ns = (self.overflow_tick as u128 * 1_000_000_000 / PM_TIMER_HZ as u128) as u64;
        if now >= overflow_ns {
            self.pm1_sts |= pm1::TMR;
        }
        self.pm1_sts
    }

    fn write_pm1_sts(&mut self, val: u16, now: Nanos) {
        let sts = self.pm1_sts(now);
        if sts & val & pm1::TMR != 0 {
            // Next overflow: the next multiple of 2^23 ticks.
            self.overflow_tick = (Self::ticks(now) + 0x80_0000) & !0x7F_FFFF;
        }
        self.pm1_sts &= !val;
    }

    fn write_pm1_en(&mut self, val: u16, now: Nanos) {
        let rising = val & !self.pm1_en;
        self.pm1_en = val;
        if rising & pm1::TMR != 0 && self.pm1_sts(now) & pm1::TMR == 0 {
            // QEMU arms a timer here that raises SCI at the next overflow.
            crate::unhandled("acpi pm timer overflow interrupt", 0x02, 2, Some(val as u64));
        }
    }

    /// QEMU `acpi_pm1_cnt_write`: SLP_EN never latches; setting it requests
    /// a sleep state, which this machine does not support.
    fn write_pm1_cnt(&mut self, val: u16) {
        self.pm1_cnt = val & !pm1_cnt::SLP_EN;
        if val & pm1_cnt::SLP_EN != 0 {
            let slp_typ = (val >> 10) & 7;
            crate::unhandled("acpi sleep request", slp_typ as u64, 2, Some(val as u64));
        }
    }

    /// Read `size` bytes at `offset` within the PM block.
    pub fn io_read(&mut self, offset: u16, size: u8, now: Nanos) -> u32 {
        let mask = size_mask(size);
        match offset {
            0x00..=0x03 => {
                if !valid(offset, size, 2) {
                    return 0;
                }
                match offset {
                    0 => self.pm1_sts(now) as u32 & mask,
                    2 => self.pm1_en as u32 & mask,
                    _ => 0,
                }
            }
            0x04..=0x05 => {
                if !valid(offset, size, 2) {
                    return 0;
                }
                (self.pm1_cnt >> (8 * (offset - 4))) as u32 & mask
            }
            0x08..=0x0B => {
                if !valid(offset, size, 4) {
                    return 0;
                }
                Self::timer(now) & mask
            }
            _ => {
                crate::unhandled("acpi pm read", offset as u64, size, None);
                mask
            }
        }
    }

    /// Write `size` bytes at `offset` within the PM block.
    pub fn io_write(&mut self, offset: u16, size: u8, val: u32, now: Nanos) {
        let val = val & size_mask(size);
        match offset {
            0x00..=0x03 => {
                if !valid(offset, size, 2) {
                    return;
                }
                match offset {
                    0 => self.write_pm1_sts(val as u16, now),
                    2 => self.write_pm1_en(val as u16, now),
                    _ => {}
                }
            }
            0x04..=0x05 => {
                if !valid(offset, size, 2) {
                    return;
                }
                let v = if offset == 5 {
                    (val as u16) << 8 | (self.pm1_cnt & 0xFF)
                } else {
                    val as u16
                };
                self.write_pm1_cnt(v);
            }
            // PM_TMR is read-only; QEMU ignores writes.
            0x08..=0x0B => {}
            _ => crate::unhandled("acpi pm write", offset as u64, size, Some(val as u64)),
        }
    }

    /// 0xB2 (APM control), 0xB3 (APM status).
    pub fn apm_read(&mut self, port: u16) -> u8 {
        if port & 1 == 0 {
            self.apmc
        } else {
            self.apms
        }
    }

    /// ACPI enable/disable commands toggle SCI_EN as QEMU; SMI is not
    /// modelled (smm=off). With smm=off QEMU presets DEVACTB.APMC_EN, so any
    /// other command would raise an SMI there; SeaBIOS never issues one in
    /// that configuration.
    pub fn apm_write(&mut self, port: u16, val: u8) {
        if port & 1 == 0 {
            self.apmc = val;
            match val {
                ACPI_ENABLE => self.pm1_cnt |= pm1_cnt::SCI_EN,
                ACPI_DISABLE => self.pm1_cnt &= !pm1_cnt::SCI_EN,
                _ => crate::unhandled("apm smi command", port as u64, 1, Some(val as u64)),
            }
        } else {
            self.apms = val;
        }
    }

    /// SCI level (IRQ 9), as QEMU's `pm_update_sci`: an enabled RTC, power
    /// button, global lock or timer status bit. Like QEMU it ignores SCI_EN.
    ///
    /// Simplification: TMR_STS is only re-evaluated when the guest accesses
    /// the PM1 registers, so a timer overflow does not raise SCI by itself
    /// (enabling that interrupt is reported through `unhandled`).
    pub fn sci_level(&self) -> bool {
        let mask = pm1::RTC | pm1::PWRBTN | pm1::GBL | pm1::TMR;
        self.pm1_sts & self.pm1_en & mask != 0
    }

    /// Current PM1_CNT value (bit 0 = SCI_EN).
    pub fn pm1_cnt(&self) -> u16 {
        self.pm1_cnt
    }
}

fn size_mask(size: u8) -> u32 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => 0xFFFF_FFFF,
    }
}

/// QEMU `memory_region_access_valid` for a region with the given maximum
/// access size and no unaligned support.
fn valid(offset: u16, size: u8, max: u8) -> bool {
    matches!(size, 1 | 2 | 4) && size <= max && offset & (size as u16 - 1) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: Nanos = 1_000_000_000;

    #[test]
    fn timer_rate_and_wrap() {
        let mut pm = Piix4Pm::new();
        assert_eq!(pm.io_read(8, 4, 0), 0);
        assert_eq!(pm.io_read(8, 4, SEC), 3_579_545);
        // 2^24 ticks take ~4.687 s; the counter wraps to 0 there.
        let wrap_ns = ((1u128 << 24) * SEC as u128).div_ceil(PM_TIMER_HZ as u128) as u64;
        assert_eq!(pm.io_read(8, 4, wrap_ns - 1), 0xFF_FFFF);
        assert_eq!(pm.io_read(8, 4, wrap_ns), 0);
        assert_eq!(pm.io_read(8, 4, 10 * SEC), (35_795_450 & 0xFF_FFFF) as u32);
        // Narrow reads return the low bits of the whole counter (QEMU).
        assert_eq!(pm.io_read(8, 2, SEC), 3_579_545 & 0xFFFF);
        assert_eq!(pm.io_read(9, 1, SEC), 3_579_545 & 0xFF);
        assert_eq!(pm.io_read(0xA, 4, SEC), 0, "misaligned");
        pm.io_write(8, 4, 0, SEC);
        assert_eq!(pm.io_read(8, 4, SEC), 3_579_545);
    }

    #[test]
    fn pm1_registers() {
        let mut pm = Piix4Pm::new();
        // QEMU's overflow time starts at 0, so TMR_STS is already set.
        assert_eq!(pm.io_read(0, 2, 0), 1);
        pm.io_write(0, 2, 1, 1000);
        assert_eq!(pm.io_read(0, 2, 1000), 0);
        let overflow_ns = ((1u128 << 23) * SEC as u128 / PM_TIMER_HZ as u128) as u64;
        assert_eq!(pm.io_read(0, 2, overflow_ns - 1), 0);
        assert_eq!(pm.io_read(0, 2, overflow_ns), 1);

        pm.io_write(2, 2, 0x0120, 0);
        assert_eq!(pm.io_read(2, 2, 0), 0x0120);
        assert_eq!(pm.io_read(3, 1, 0), 0, "odd bytes of PM1_EVT read 0 in QEMU");
        assert_eq!(pm.io_read(0, 4, 0), 0, "PM1_EVT rejects dword access");
        assert!(!pm.sci_level());

        pm.io_write(4, 2, 0x0001, 0);
        assert_eq!(pm.io_read(4, 2, 0), 1);
        pm.io_write(5, 1, 0x02, 0);
        assert_eq!(pm.io_read(4, 2, 0), 0x0201);
        assert_eq!(pm.io_read(5, 1, 0), 0x02);
        // SLP_EN is write-only.
        pm.io_write(4, 2, 0x2001, 0);
        assert_eq!(pm.io_read(4, 2, 0), 0x0001);

        // Unassigned space inside the block reads all-ones.
        assert_eq!(pm.io_read(0x28, 4, 0), 0xFFFF_FFFF);
        assert_eq!(pm.io_read(0x06, 1, 0), 0xFF);
    }

    #[test]
    fn sci_from_timer_status() {
        let mut pm = Piix4Pm::new();
        pm.io_read(0, 2, 0);
        pm.io_write(2, 2, pm1::TMR as u32, 0);
        assert!(pm.sci_level());
        pm.io_write(0, 2, pm1::TMR as u32, 0);
        assert!(!pm.sci_level());
    }

    #[test]
    fn apm_ports() {
        let mut pm = Piix4Pm::new();
        pm.apm_write(APM_STS_PORT, 0x5A);
        assert_eq!(pm.apm_read(APM_STS_PORT), 0x5A);
        pm.apm_write(APM_CNT_PORT, ACPI_ENABLE);
        assert_eq!(pm.apm_read(APM_CNT_PORT), ACPI_ENABLE);
        assert_eq!(pm.io_read(4, 2, 0) & 1, 1);
        pm.apm_write(APM_CNT_PORT, ACPI_DISABLE);
        assert_eq!(pm.io_read(4, 2, 0) & 1, 0);
    }
}
