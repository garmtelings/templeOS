//! MC146818 RTC and 128-byte CMOS NVRAM (ports 0x70/0x71), after QEMU's
//! `hw/rtc/mc146818rtc.c`.
//!
//! As in QEMU, the clock is not ticked: guest time is kept as a base (unix
//! seconds) plus the machine time elapsed since that base was set, and the
//! time registers are rewritten from it whenever the guest reads one while
//! the clock runs. UIP is derived from the sub-second phase (set for the
//! last 244 µs of each second), and a SET/divider-reset clock freezes the
//! registers, with guest writes taking effect when it is released.
//!
//! Simplifications: interrupts (periodic, alarm, update-ended) are not
//! delivered and the square-wave output is not modelled; the guest
//! enabling PIE/AIE/UIE is reported through [`crate::unhandled`]. The UF and
//! AF flags in register C are still latched once per crossed second, so
//! register C reads like QEMU's; AF uses the alarm registers' own PM bit in
//! 12-hour mode, and PF is never set (QEMU only sets it with PIE or SQWE).

use crate::{unhandled, Nanos};

const NS: i128 = 1_000_000_000;
/// QEMU `UIP_HOLD_LENGTH`: 8 cycles of the 32.768 kHz time base.
const UIP_HOLD_NS: i128 = 8 * NS / 32768;

const SECONDS: usize = 0x00;
const SECONDS_ALARM: usize = 0x01;
const MINUTES: usize = 0x02;
const MINUTES_ALARM: usize = 0x03;
const HOURS: usize = 0x04;
const HOURS_ALARM: usize = 0x05;
const DAY_OF_WEEK: usize = 0x06;
const DAY_OF_MONTH: usize = 0x07;
const MONTH: usize = 0x08;
const YEAR: usize = 0x09;
const REG_A: usize = 0x0A;
const REG_B: usize = 0x0B;
const REG_C: usize = 0x0C;
const REG_D: usize = 0x0D;
const CENTURY: usize = 0x32;
/// Alias QEMU redirects to the century register (and moves the index to it).
const IBM_PS2_CENTURY: usize = 0x37;

const A_UIP: u8 = 0x80;
const B_SET: u8 = 0x80;
const B_UIE: u8 = 0x10;
const B_IRQ_ENABLES: u8 = 0x70;
const B_DM: u8 = 0x04;
const B_24H: u8 = 0x02;
const C_IRQF: u8 = 0x80;
const C_AF: u8 = 0x20;
const C_UF: u8 = 0x10;

/// Broken-down UTC time. Fields are wide and signed because they may come
/// from arbitrary guest register contents (QEMU decodes "don't care"
/// values as -1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tm {
    year: i64,
    /// 1..=12
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    /// 0 = Sunday
    weekday: i64,
}

/// Unix seconds to UTC (proleptic Gregorian, any sign).
fn gmtime(secs: i64) -> Tm {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    Tm {
        year,
        month,
        day,
        hour: tod / 3600,
        minute: tod / 60 % 60,
        second: tod % 60,
        weekday: (days + 4).rem_euclid(7), // 1970-01-01 was a Thursday
    }
}

/// UTC to unix seconds, exactly as QEMU's `mktimegm` (ignores the weekday,
/// lets out-of-range fields spill over).
fn mktimegm(tm: &Tm) -> i64 {
    let (mut y, mut m) = (tm.year, tm.month);
    if m < 3 {
        m += 12;
        y -= 1;
    }
    let days = tm.day + (153 * m - 457) / 5 + 365 * y + y / 4 - y / 100 + y / 400 - 719_469;
    days.wrapping_mul(86_400) + 3600 * tm.hour + 60 * tm.minute + tm.second
}

/// The CMOS RTC.
#[derive(Clone, Debug)]
pub struct Rtc {
    cmos: [u8; 128],
    index: u8,
    nmi_masked: bool,
    /// Guest time = base_rtc s + (now - last_update) + offset.
    base_rtc: i64,
    last_update: Nanos,
    offset: i128,
    /// Guest second up to which UF/AF have been latched into register C.
    flags_sec: i128,
    irq_enable_reported: bool,
}

impl Rtc {
    /// Guest wall clock = `base_unix_secs` + `now`, starting exactly on a
    /// second boundary. Registers A=0x26, B=0x02 (24-hour, BCD), C=0,
    /// D=0x80, time and century (0x32) registers set from the base; the
    /// rest of the NVRAM is zero until the board fills it.
    pub fn new(base_unix_secs: i64) -> Self {
        let mut rtc = Rtc {
            cmos: [0; 128],
            index: 0,
            nmi_masked: false,
            base_rtc: base_unix_secs,
            last_update: 0,
            offset: 0,
            flags_sec: base_unix_secs as i128,
            irq_enable_reported: false,
        };
        rtc.cmos[REG_A] = 0x26;
        rtc.cmos[REG_B] = B_24H;
        rtc.cmos[REG_D] = 0x80;
        rtc.set_cmos(&gmtime(base_unix_secs));
        rtc
    }

    /// Store a raw NVRAM byte (index & 0x7F) with no side effects.
    pub fn set_nvram(&mut self, index: u8, val: u8) {
        self.cmos[(index & 0x7F) as usize] = val;
    }

    /// Raw NVRAM byte (index & 0x7F), without refreshing the clock.
    pub fn nvram(&self, index: u8) -> u8 {
        self.cmos[(index & 0x7F) as usize]
    }

    /// Guest port read. 0x70 reads 0xFF (as QEMU); 0x71 reads the selected
    /// register: time registers are brought up to date first unless the
    /// clock is stopped, register A carries the live UIP bit, register C is
    /// cleared by the read.
    pub fn read(&mut self, port: u16, now: Nanos) -> u8 {
        match port {
            0x70 => 0xFF,
            0x71 => self.read_data(now),
            _ => {
                unhandled("rtc io read", port as u64, 1, None);
                0xFF
            }
        }
    }

    /// Guest port write. 0x70 selects the register (val & 0x7F) and sets the
    /// NMI mask from bit 7; 0x71 writes the selected register.
    pub fn write(&mut self, port: u16, val: u8, now: Nanos) {
        match port {
            0x70 => {
                self.index = val & 0x7F;
                self.nmi_masked = val & 0x80 != 0;
            }
            0x71 => self.write_data(val, now),
            _ => unhandled("rtc io write", port as u64, 1, Some(val as u64)),
        }
    }

    /// NMI disable bit from the last index write to port 0x70.
    pub fn nmi_masked(&self) -> bool {
        self.nmi_masked
    }

    fn read_data(&mut self, now: Nanos) -> u8 {
        self.sync_flags(now);
        let mut idx = self.index as usize;
        if idx == IBM_PS2_CENTURY {
            self.index = CENTURY as u8;
            idx = CENTURY;
        }
        match idx {
            SECONDS | MINUTES | HOURS | DAY_OF_WEEK | DAY_OF_MONTH | MONTH | YEAR | CENTURY => {
                if self.running() {
                    self.update_time(now);
                }
                self.cmos[idx]
            }
            REG_A => {
                let uip = if self.update_in_progress(now) { A_UIP } else { 0 };
                self.cmos[REG_A] | uip
            }
            REG_C => std::mem::take(&mut self.cmos[REG_C]),
            _ => self.cmos[idx],
        }
    }

    fn write_data(&mut self, val: u8, now: Nanos) {
        self.sync_flags(now);
        let mut idx = self.index as usize;
        if idx == IBM_PS2_CENTURY {
            self.index = CENTURY as u8;
            idx = CENTURY;
        }
        match idx {
            SECONDS | MINUTES | HOURS | DAY_OF_WEEK | DAY_OF_MONTH | MONTH | YEAR | CENTURY => {
                self.cmos[idx] = val;
                // While stopped the write waits in the register.
                if self.running() {
                    self.set_time(now);
                }
            }
            REG_A => {
                let a = self.cmos[REG_A];
                if val & 0x60 == 0x60 {
                    // Divider reset: freeze the registers.
                    if self.running() {
                        self.update_time(now);
                    }
                } else if a & 0x60 == 0x60 && val & 0x70 <= 0x20 {
                    // Leaving divider reset: the first update comes 0.5 s later.
                    if self.cmos[REG_B] & B_SET == 0 {
                        self.offset = NS / 2;
                        self.set_time(now);
                    }
                }
                self.cmos[REG_A] = val & !A_UIP;
            }
            REG_B => {
                let mut data = val;
                if data & B_SET != 0 {
                    if self.running() {
                        self.update_time(now);
                    }
                    data &= !B_UIE;
                } else if self.cmos[REG_B] & B_SET != 0 && self.cmos[REG_A] & 0x70 <= 0x20 {
                    // Leaving SET: restart from the (possibly rewritten)
                    // registers, keeping the sub-second phase.
                    self.offset = self.guest_ns(now).rem_euclid(NS);
                    self.set_time(now);
                }
                if data & self.cmos[REG_C] & B_IRQ_ENABLES != 0 {
                    self.cmos[REG_C] |= C_IRQF;
                } else {
                    self.cmos[REG_C] &= !C_IRQF;
                }
                self.cmos[REG_B] = data;
                if data & B_IRQ_ENABLES != 0 && !self.irq_enable_reported {
                    self.irq_enable_reported = true;
                    unhandled("rtc irq enable", REG_B as u64, 1, Some(data as u64));
                }
            }
            REG_C | REG_D => {}
            _ => self.cmos[idx] = val,
        }
    }

    fn guest_ns(&self, now: Nanos) -> i128 {
        self.base_rtc as i128 * NS + (now as i128 - self.last_update as i128) + self.offset
    }

    fn guest_sec(&self, now: Nanos) -> i128 {
        self.guest_ns(now).div_euclid(NS)
    }

    /// Updating: SET clear and the divider in a counting configuration.
    fn running(&self) -> bool {
        self.cmos[REG_B] & B_SET == 0 && self.cmos[REG_A] & 0x70 <= 0x20
    }

    fn update_in_progress(&self, now: Nanos) -> bool {
        self.running() && self.guest_ns(now).rem_euclid(NS) >= NS - UIP_HOLD_NS
    }

    /// QEMU `rtc_update_time`: registers <- guest clock.
    fn update_time(&mut self, now: Nanos) {
        if self.cmos[REG_B] & B_SET == 0 {
            let secs = self.guest_sec(now).clamp(-(1 << 50), 1 << 50) as i64;
            self.set_cmos(&gmtime(secs));
        }
    }

    /// QEMU `rtc_set_time`: guest clock <- registers (sub-second offset kept).
    fn set_time(&mut self, now: Nanos) {
        self.base_rtc = mktimegm(&self.get_time());
        self.last_update = now;
        self.flags_sec = self.guest_sec(now);
    }

    fn encode_bcd(&self, a: i64) -> u8 {
        if self.cmos[REG_B] & B_DM != 0 {
            a as u8
        } else {
            (((a / 10) << 4) | (a % 10)) as u8
        }
    }

    /// Register value to a number; 0xC0-0xFF ("don't care") decode as -1.
    fn decode_bcd(&self, a: u8) -> i64 {
        if a & 0xC0 == 0xC0 {
            -1
        } else if self.cmos[REG_B] & B_DM != 0 {
            a as i64
        } else {
            (a >> 4) as i64 * 10 + (a & 0x0F) as i64
        }
    }

    /// Hour register (either format) to 0..=23.
    fn decode_hour(&self, reg: u8) -> i64 {
        let mut h = self.decode_bcd(reg & 0x7F);
        if self.cmos[REG_B] & B_24H == 0 {
            h %= 12;
            if reg & 0x80 != 0 {
                h += 12;
            }
        }
        h
    }

    fn set_cmos(&mut self, tm: &Tm) {
        self.cmos[SECONDS] = self.encode_bcd(tm.second);
        self.cmos[MINUTES] = self.encode_bcd(tm.minute);
        self.cmos[HOURS] = if self.cmos[REG_B] & B_24H != 0 {
            self.encode_bcd(tm.hour)
        } else {
            let h = if tm.hour % 12 != 0 { tm.hour % 12 } else { 12 };
            self.encode_bcd(h) | if tm.hour >= 12 { 0x80 } else { 0 }
        };
        self.cmos[DAY_OF_WEEK] = self.encode_bcd(tm.weekday + 1);
        self.cmos[DAY_OF_MONTH] = self.encode_bcd(tm.day);
        self.cmos[MONTH] = self.encode_bcd(tm.month);
        self.cmos[YEAR] = self.encode_bcd(tm.year % 100);
        self.cmos[CENTURY] = self.encode_bcd(tm.year / 100);
    }

    fn get_time(&self) -> Tm {
        Tm {
            second: self.decode_bcd(self.cmos[SECONDS]),
            minute: self.decode_bcd(self.cmos[MINUTES]),
            hour: self.decode_hour(self.cmos[HOURS]),
            weekday: self.decode_bcd(self.cmos[DAY_OF_WEEK]) - 1,
            day: self.decode_bcd(self.cmos[DAY_OF_MONTH]),
            month: self.decode_bcd(self.cmos[MONTH]),
            year: self.decode_bcd(self.cmos[YEAR]) + self.decode_bcd(self.cmos[CENTURY]) * 100,
        }
    }

    /// Latch UF (and AF, if the alarm matched) for every second boundary
    /// crossed since the last check, as QEMU's update timer does. Nothing
    /// latches while the divider is held in reset.
    fn sync_flags(&mut self, now: Nanos) {
        let cur = self.guest_sec(now);
        if self.cmos[REG_A] & 0x60 == 0x60 {
            self.flags_sec = cur;
            return;
        }
        if cur <= self.flags_sec {
            return;
        }
        let mut irqs = C_UF;
        // Every time of day occurs within any 86400 consecutive seconds.
        let first = (self.flags_sec + 1).max(cur - 86_399);
        if (first..=cur).any(|s| self.alarm_matches(s)) {
            irqs |= C_AF;
        }
        let new = irqs & !self.cmos[REG_C];
        self.cmos[REG_C] |= irqs;
        if new & self.cmos[REG_B] & B_IRQ_ENABLES != 0 {
            self.cmos[REG_C] |= C_IRQF;
        }
        self.flags_sec = cur;
    }

    fn alarm_matches(&self, guest_sec: i128) -> bool {
        let tod = guest_sec.rem_euclid(86_400) as i64;
        let field = |reg: usize, value: i64| {
            let raw = self.cmos[reg];
            if raw & 0xC0 == 0xC0 {
                return true;
            }
            let want = if reg == HOURS_ALARM {
                self.decode_hour(raw)
            } else {
                self.decode_bcd(raw)
            };
            want == value
        };
        field(SECONDS_ALARM, tod % 60)
            && field(MINUTES_ALARM, tod / 60 % 60)
            && field(HOURS_ALARM, tod / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tue 2023-11-14 22:13:20 UTC.
    const BASE: i64 = 1_700_000_000;
    const SEC: Nanos = 1_000_000_000;

    fn reg(rtc: &mut Rtc, idx: u8, now: Nanos) -> u8 {
        rtc.write(0x70, idx, now);
        rtc.read(0x71, now)
    }

    fn set(rtc: &mut Rtc, idx: u8, val: u8, now: Nanos) {
        rtc.write(0x70, idx, now);
        rtc.write(0x71, val, now);
    }

    fn time_regs(rtc: &mut Rtc, now: Nanos) -> [u8; 8] {
        [0x00, 0x02, 0x04, 0x06, 0x07, 0x08, 0x09, 0x32].map(|i| reg(rtc, i, now))
    }

    #[test]
    fn date_math() {
        let t = gmtime(BASE);
        assert_eq!(
            t,
            Tm { year: 2023, month: 11, day: 14, hour: 22, minute: 13, second: 20, weekday: 2 }
        );
        assert_eq!(gmtime(0).weekday, 4);
        let leap = gmtime(951_782_400); // 2000-02-29
        assert_eq!((leap.year, leap.month, leap.day), (2000, 2, 29));
        let old = gmtime(-1);
        assert_eq!((old.year, old.month, old.day, old.hour, old.second), (1969, 12, 31, 23, 59));
        for secs in [0, BASE, -86_400 * 365 * 70, 951_782_400, 4_102_444_800, 253_402_300_799] {
            assert_eq!(mktimegm(&gmtime(secs)), secs);
        }
    }

    #[test]
    fn reset_registers_bcd() {
        let mut rtc = Rtc::new(BASE);
        assert_eq!(time_regs(&mut rtc, 0), [0x20, 0x13, 0x22, 0x03, 0x14, 0x11, 0x23, 0x20]);
        assert_eq!(reg(&mut rtc, 0x0A, 0), 0x26);
        assert_eq!(reg(&mut rtc, 0x0B, 0), 0x02);
        assert_eq!(reg(&mut rtc, 0x0C, 0), 0x00);
        assert_eq!(reg(&mut rtc, 0x0D, 0), 0x80);
        assert_eq!(time_regs(&mut rtc, 41 * SEC)[..2], [0x01, 0x14]);
    }

    #[test]
    fn binary_and_12_hour() {
        let mut rtc = Rtc::new(BASE);
        set(&mut rtc, 0x0B, 0x06, 0);
        assert_eq!(time_regs(&mut rtc, 0), [20, 13, 22, 3, 14, 11, 23, 20]);
        set(&mut rtc, 0x0B, 0x00, 0);
        assert_eq!(reg(&mut rtc, 0x04, 0), 0x90, "10 PM in BCD");
        set(&mut rtc, 0x0B, 0x04, 0);
        assert_eq!(reg(&mut rtc, 0x04, 0), 0x8A, "10 PM in binary");
    }

    #[test]
    fn uip_window() {
        let mut rtc = Rtc::new(BASE);
        let edge = 3 * SEC - 244_140;
        assert_eq!(reg(&mut rtc, 0x0A, edge - 1) & 0x80, 0);
        assert_eq!(reg(&mut rtc, 0x0A, edge), 0x80 | 0x26);
        assert_eq!(reg(&mut rtc, 0x0A, 3 * SEC - 1) & 0x80, 0x80);
        assert_eq!(reg(&mut rtc, 0x0A, 3 * SEC), 0x26);
        // No UIP while SET is on.
        set(&mut rtc, 0x0B, 0x82, 3 * SEC);
        assert_eq!(reg(&mut rtc, 0x0A, edge + SEC) & 0x80, 0);
    }

    #[test]
    fn set_bit_freezes_and_writes_back() {
        let mut rtc = Rtc::new(BASE);
        let half = SEC / 2;
        set(&mut rtc, 0x0B, 0x82, half);
        set(&mut rtc, 0x04, 0x05, half);
        set(&mut rtc, 0x02, 0x59, half);
        assert_eq!(reg(&mut rtc, 0x00, 2 * SEC), 0x20, "frozen");
        assert_eq!(reg(&mut rtc, 0x04, 2 * SEC), 0x05);
        set(&mut rtc, 0x0B, 0x02, 2 * SEC + half);
        // Released at phase .5, so the first tick comes 0.5 s later.
        assert_eq!(time_regs(&mut rtc, 2 * SEC + 900_000_000)[..3], [0x20, 0x59, 0x05]);
        assert_eq!(time_regs(&mut rtc, 3 * SEC + 100_000_000)[..3], [0x21, 0x59, 0x05]);
    }

    #[test]
    fn running_write_takes_effect_immediately() {
        let mut rtc = Rtc::new(BASE);
        set(&mut rtc, 0x09, 0x99, SEC);
        set(&mut rtc, 0x32, 0x19, SEC);
        assert_eq!(reg(&mut rtc, 0x09, 2 * SEC), 0x99);
        assert_eq!(reg(&mut rtc, 0x32, 2 * SEC), 0x19);
        assert_eq!(reg(&mut rtc, 0x37, 2 * SEC), 0x19, "PS/2 century alias");
        assert_eq!(rtc.index, 0x32);
    }

    #[test]
    fn reg_c_clears_on_read() {
        let mut rtc = Rtc::new(BASE);
        assert_eq!(reg(&mut rtc, 0x0C, SEC / 2), 0);
        assert_eq!(reg(&mut rtc, 0x0C, SEC + SEC / 2), 0x10, "update-ended flag");
        assert_eq!(reg(&mut rtc, 0x0C, SEC + SEC / 2), 0);
        // Alarm at 22:13:25 (BCD).
        set(&mut rtc, 0x01, 0x25, 2 * SEC);
        set(&mut rtc, 0x03, 0x13, 2 * SEC);
        set(&mut rtc, 0x05, 0x22, 2 * SEC);
        reg(&mut rtc, 0x0C, 2 * SEC);
        assert_eq!(reg(&mut rtc, 0x0C, 4 * SEC), 0x10);
        assert_eq!(reg(&mut rtc, 0x0C, 5 * SEC), 0x30);
        set(&mut rtc, 0x0C, 0xFF, 5 * SEC);
        assert_eq!(reg(&mut rtc, 0x0C, 5 * SEC), 0, "read-only");
    }

    #[test]
    fn index_port_and_nmi() {
        let mut rtc = Rtc::new(BASE);
        assert_eq!(rtc.read(0x70, 0), 0xFF);
        rtc.write(0x70, 0x8F, 0);
        assert!(rtc.nmi_masked());
        rtc.write(0x71, 0x00, 0);
        assert_eq!(rtc.nvram(0x0F), 0);
        rtc.write(0x70, 0x0F, 0);
        assert!(!rtc.nmi_masked());
        rtc.set_nvram(0x34, 0xAB);
        assert_eq!(reg(&mut rtc, 0x34, 0), 0xAB);
    }

    #[test]
    fn divider_reset_restarts_half_second_later() {
        let mut rtc = Rtc::new(BASE);
        set(&mut rtc, 0x0A, 0x76, SEC / 4);
        assert_eq!(reg(&mut rtc, 0x00, 10 * SEC), 0x20, "stopped");
        set(&mut rtc, 0x0A, 0x26, 10 * SEC);
        assert_eq!(reg(&mut rtc, 0x00, 10 * SEC + 499_000_000), 0x20);
        assert_eq!(reg(&mut rtc, 0x00, 10 * SEC + 500_000_000), 0x21);
    }
}
