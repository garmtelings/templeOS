//! HPET (QEMU `hw/timer/hpet.c`, `-machine pc,hpet=on`).
//!
//! TempleOS reads GCAP_ID, sets GEN_CONF.ENABLE and then reads MAIN_CNT as
//! its main clock; it never uses the comparators. The register file is
//! modelled completely so every read returns what QEMU would, but comparator
//! interrupts are not: enabling one is reported through [`crate::unhandled`].
//!
//! QEMU's region only accepts aligned 4-byte accesses; 8-byte guest accesses
//! reach it as two 4-byte accesses, low half first. This model does the same
//! split, with both halves at the same `now` (so a 64-bit counter read can't
//! tear, which QEMU only avoids by luck of timing).

use crate::Nanos;

pub const HPET_BASE: u64 = 0xFED0_0000;
pub const HPET_SIZE: u64 = 0x400;

/// Main counter period in femtoseconds (10 ns, 100 MHz).
pub const PERIOD_FS: u64 = 10_000_000;
const NS_PER_TICK: u64 = 10;
const NUM_TIMERS: usize = 3;

/// GCAP_ID: 10 ns period, vendor 8086, legacy-route capable, 64-bit counter,
/// 3 timers (NUM_TIM_CAP = 2), revision 1.
pub const CAPABILITIES: u64 = 0x0098_9680_8086_A201;

const GCAP_ID: u32 = 0x000;
const GEN_CONF: u32 = 0x010;
const GINTR_STA: u32 = 0x020;
const MAIN_CNT: u32 = 0x0F0;

const CFG_ENABLE: u64 = 1 << 0;
const CFG_LEGACY: u64 = 1 << 1;
const CFG_WRITE_MASK: u64 = 3;

const TN_ENABLE: u64 = 1 << 2;
const TN_PERIODIC: u64 = 1 << 3;
const TN_PERIODIC_CAP: u64 = 1 << 4;
const TN_SIZE_CAP: u64 = 1 << 5;
const TN_SETVAL: u64 = 1 << 6;
const TN_32BIT: u64 = 1 << 8;
const TN_FSB_ENABLE: u64 = 1 << 14;
const TN_CFG_WRITE_MASK: u64 = 0x7F4E;
/// Tn_INT_ROUTE_CAP: the i440FX machine only advertises IRQ 2.
const TN_INT_ROUTE_CAP: u64 = 0x4 << 32;

#[derive(Clone, Copy)]
struct Timer {
    config: u64,
    cmp: u64,
    period: u64,
    fsb: u64,
}

impl Timer {
    fn reset() -> Self {
        Timer {
            config: TN_PERIODIC_CAP | TN_SIZE_CAP | TN_INT_ROUTE_CAP,
            cmp: !0,
            period: 0,
            fsb: 0,
        }
    }

    fn periodic(&self) -> bool {
        self.config & TN_PERIODIC != 0
    }
}

/// HPET at 0xFED00000 (1 KiB). Capabilities exactly 0x0098_9680_8086_A201.
///
/// The main counter runs at 100 MHz only while GEN_CONF.ENABLE is set. As in
/// QEMU, a MAIN_CNT write while the counter runs has no visible effect: it is
/// overwritten by the running value when the counter is next stopped.
///
/// Simplification: QEMU advances a periodic comparator each time it expires
/// even with its interrupt disabled; here comparators only change on writes.
pub struct Hpet {
    config: u64,
    isr: u64,
    /// Counter value while halted (QEMU `hpet_counter`).
    counter: u64,
    /// `ticks = (now + offset) / 10` while enabled (QEMU `hpet_offset`).
    offset: u64,
    timers: [Timer; NUM_TIMERS],
}

impl Default for Hpet {
    fn default() -> Self {
        Self::new()
    }
}

impl Hpet {
    pub fn new() -> Self {
        Hpet { config: 0, isr: 0, counter: 0, offset: 0, timers: [Timer::reset(); NUM_TIMERS] }
    }

    fn enabled(&self) -> bool {
        self.config & CFG_ENABLE != 0
    }

    /// MAIN_CNT at `now`.
    pub fn counter(&self, now: Nanos) -> u64 {
        if self.enabled() {
            now.wrapping_add(self.offset) / NS_PER_TICK
        } else {
            self.counter
        }
    }

    /// Read at `offset` (4- and 8-byte accesses). Other sizes and misaligned
    /// accesses are invalid in QEMU and read 0.
    pub fn read(&mut self, offset: u32, size: u8, now: Nanos) -> u64 {
        match size {
            4 if offset.is_multiple_of(4) => self.read32(offset, now) as u64,
            8 if offset.is_multiple_of(8) => {
                self.read32(offset, now) as u64 | (self.read32(offset + 4, now) as u64) << 32
            }
            _ => {
                crate::unhandled("hpet read", HPET_BASE + offset as u64, size, None);
                0
            }
        }
    }

    /// Write at `offset` (4- and 8-byte accesses).
    pub fn write(&mut self, offset: u32, size: u8, val: u64, now: Nanos) {
        match size {
            4 if offset.is_multiple_of(4) => self.write32(offset, val as u32, now),
            8 if offset.is_multiple_of(8) => {
                self.write32(offset, val as u32, now);
                self.write32(offset + 4, (val >> 32) as u32, now);
            }
            _ => crate::unhandled("hpet write", HPET_BASE + offset as u64, size, Some(val)),
        }
    }

    /// Timer index and register offset within the timer block, if `offset`
    /// addresses one of the implemented timers.
    fn timer_reg(offset: u32) -> Option<(usize, u32)> {
        if !(0x100..0x400).contains(&offset) {
            return None;
        }
        let n = ((offset - 0x100) / 0x20) as usize;
        (n < NUM_TIMERS).then_some((n, (offset - 0x100) % 0x20))
    }

    fn read32(&self, offset: u32, now: Nanos) -> u32 {
        if (0x100..0x400).contains(&offset) {
            // Timers beyond the third read 0. (QEMU's off-by-one range check
            // exposes an unused, zeroed fourth timer at 0x160; same result.)
            let Some((n, r)) = Self::timer_reg(offset) else { return 0 };
            let t = &self.timers[n];
            let v = match r & !4 {
                0x00 => t.config,
                0x08 => t.cmp,
                0x10 => t.fsb,
                _ => return 0,
            };
            return (v >> (8 * (r & 4))) as u32;
        }
        match offset {
            GCAP_ID => CAPABILITIES as u32,
            0x004 => (CAPABILITIES >> 32) as u32,
            GEN_CONF => self.config as u32,
            GINTR_STA => self.isr as u32,
            MAIN_CNT => self.counter(now) as u32,
            0x0F4 => (self.counter(now) >> 32) as u32,
            _ => 0,
        }
    }

    fn write32(&mut self, offset: u32, val: u32, now: Nanos) {
        let new = val as u64;
        if (0x100..0x400).contains(&offset) {
            let Some((n, r)) = Self::timer_reg(offset) else {
                crate::unhandled("hpet write", HPET_BASE + offset as u64, 4, Some(new));
                return;
            };
            self.write_timer(n, r, new, offset);
            return;
        }
        match offset {
            GEN_CONF => {
                let old = self.config;
                self.config = (old & !CFG_WRITE_MASK) | (new & CFG_WRITE_MASK);
                if old & CFG_ENABLE == 0 && new & CFG_ENABLE != 0 {
                    self.offset = self.counter.wrapping_mul(NS_PER_TICK).wrapping_sub(now);
                    self.check_timers(offset);
                } else if old & CFG_ENABLE != 0 && new & CFG_ENABLE == 0 {
                    self.counter = now.wrapping_add(self.offset) / NS_PER_TICK;
                }
                if (old ^ new) & CFG_LEGACY != 0 {
                    // Legacy replacement routing would take IRQ0/IRQ8 away
                    // from the PIT and RTC.
                    crate::unhandled("hpet legacy route", HPET_BASE + offset as u64, 4, Some(new));
                }
            }
            // Write 1 to clear. ISR bits are never set here because timer
            // interrupts are not modelled.
            GINTR_STA => self.isr &= !(new & self.isr),
            MAIN_CNT => self.counter = (self.counter & !0xFFFF_FFFF) | new,
            0x0F4 => self.counter = (self.counter & 0xFFFF_FFFF) | new << 32,
            _ => {} // GCAP_ID and reserved registers are read-only.
        }
    }

    fn write_timer(&mut self, n: usize, r: u32, new: u64, offset: u32) {
        let t = &mut self.timers[n];
        match r {
            0x00 => {
                let old = t.config;
                t.config = (old & !TN_CFG_WRITE_MASK) | (new & TN_CFG_WRITE_MASK);
                if new & TN_32BIT != 0 {
                    t.cmp = t.cmp as u32 as u64;
                    t.period = t.period as u32 as u64;
                }
                if new & !old & (TN_ENABLE | TN_FSB_ENABLE) != 0 {
                    // Tn_INT_ENB_CNF: comparator interrupts are not modelled.
                    crate::unhandled("hpet timer interrupt", HPET_BASE + offset as u64, 4, Some(new));
                }
            }
            0x08 => {
                let mut v = new;
                if t.config & TN_32BIT != 0 {
                    v = v as u32 as u64;
                }
                if !t.periodic() || t.config & TN_SETVAL != 0 {
                    t.cmp = (t.cmp & !0xFFFF_FFFF) | v;
                }
                if t.periodic() {
                    if t.config & TN_32BIT != 0 {
                        v = v.min((!0u32 >> 1) as u64);
                    }
                    t.period = (t.period & !0xFFFF_FFFF) | v;
                }
                t.config &= !TN_SETVAL;
            }
            0x0C => {
                if t.config & TN_32BIT == 0 {
                    if !t.periodic() || t.config & TN_SETVAL != 0 {
                        t.cmp = (t.cmp & 0xFFFF_FFFF) | new << 32;
                    } else {
                        let v = new.min((!0u32 >> 1) as u64);
                        t.period = (t.period & 0xFFFF_FFFF) | v << 32;
                    }
                    t.config &= !TN_SETVAL;
                }
            }
            0x10 => t.fsb = (t.fsb & !0xFFFF_FFFF) | new,
            0x14 => t.fsb = (t.fsb & 0xFFFF_FFFF) | new << 32,
            _ => {} // Tn_CFG high half (route capabilities) is read-only.
        }
    }

    /// Enabling the counter with an armed interrupt-enabled comparator would
    /// start firing it in QEMU.
    fn check_timers(&self, offset: u32) {
        if self.timers.iter().any(|t| t.config & TN_ENABLE != 0) {
            crate::unhandled("hpet timer interrupt", HPET_BASE + offset as u64, 4, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities() {
        let mut h = Hpet::new();
        assert_eq!(h.read(0, 8, 0), 0x0098_9680_8086_A201);
        assert_eq!(h.read(0, 4, 0), 0x8086_A201);
        assert_eq!(h.read(4, 4, 0), 0x0098_9680);
        h.write(0, 8, 0, 0);
        assert_eq!(h.read(0, 8, 0), CAPABILITIES);
        assert_eq!(h.read(0x10, 8, 0), 0);
        assert_eq!(h.read(0x100, 8, 0), 0x0000_0004_0000_0030);
        assert_eq!(h.read(0x108, 8, 0), !0);
        assert_eq!(h.read(0x140, 8, 0), 0x0000_0004_0000_0030);
        assert_eq!(h.read(0x160, 8, 0), 0);
        assert_eq!(h.read(0, 2, 0), 0);
    }

    #[test]
    fn counter_frozen_while_disabled() {
        let mut h = Hpet::new();
        assert_eq!(h.read(0xF0, 8, 0), 0);
        assert_eq!(h.read(0xF0, 8, 5_000_000), 0);
        h.write(0xF0, 8, 0x1_0000_0000, 5_000_000);
        assert_eq!(h.read(0xF0, 8, 9_000_000), 0x1_0000_0000);
    }

    #[test]
    fn counter_runs_at_100mhz_when_enabled() {
        let mut h = Hpet::new();
        let t0 = 1_000_000;
        h.write(0x10, 8, 1, t0);
        assert_eq!(h.read(0x10, 8, t0), 1);
        assert_eq!(h.read(0xF0, 8, t0), 0);
        assert_eq!(h.read(0xF0, 8, t0 + 1_000_000_000), 100_000_000);
        assert_eq!(h.read(0xF0, 8, t0 + 19), 1);
        // Past 2^32 ticks (~42.9 s): 64-bit read and two 32-bit reads agree.
        let t = t0 + 50_000_000_000;
        let want = 5_000_000_000u64;
        assert_eq!(h.read(0xF0, 8, t), want);
        let lo = h.read(0xF0, 4, t);
        let hi = h.read(0xF4, 4, t);
        assert_eq!(hi << 32 | lo, want);
        // Stop, then the counter holds its value.
        h.write(0x10, 4, 0, t);
        assert_eq!(h.read(0xF0, 8, t + 1_000_000_000), want);
        // Restart continues from the held value.
        h.write(0x10, 4, 1, t + 2_000_000_000);
        assert_eq!(h.read(0xF0, 8, t + 2_000_000_100), want + 10);
    }

    #[test]
    fn timer_registers() {
        let mut h = Hpet::new();
        h.write(0x108, 8, 0x1234_5678_9ABC_DEF0, 0);
        assert_eq!(h.read(0x108, 8, 0), 0x1234_5678_9ABC_DEF0);
        h.write(0x100, 4, TN_32BIT, 0);
        assert_eq!(h.read(0x108, 8, 0), 0x9ABC_DEF0);
        assert_eq!(h.read(0x100, 8, 0), 0x0000_0004_0000_0130);
        h.write(0x20, 4, 0xFFFF_FFFF, 0);
        assert_eq!(h.read(0x20, 4, 0), 0);
    }
}
