//! Intel 8254 PIT (ports 0x40-0x43) and the PC speaker / system control
//! port B (0x61).
//!
//! Counting follows QEMU's `hw/timer/i8254.c` + `i8254_common.c`: a channel
//! does not tick; its count and output are computed from the time since it
//! was loaded (`ticks = elapsed_ns * PIT_FREQ / 1e9`, 128-bit, rounded down,
//! like `muldiv64`). Channel 0's IRQ timer is reproduced exactly, including
//! QEMU's transition times, so IRQ0 edges land where the reference machine
//! puts them.
//!
//! QEMU-isms kept on purpose because the guest can observe them:
//! - Mode 2 OUT, as seen by read-back status and port 0x61, is high only
//!   during the tick the count wraps (real 8254s are the inverse: low for
//!   that tick). IRQ0 still rises once per period.
//! - A control word does not stop the counter or null the count; the old
//!   count keeps running in the new mode until a new count is written.
//! - The status byte never reports NULL COUNT, and BCD counting is stored
//!   but not implemented.
//! - The gate is ignored in modes 0 and 4 (counting is never inhibited).
//!
//! Simplification: QEMU's HPET legacy-replacement route (which disables the
//! PIT IRQ) is not modelled; neither SeaBIOS nor TempleOS enables it.

use crate::{unhandled, Nanos};

/// Input clock of the 8254, Hz.
pub const PIT_FREQ: u64 = 1_193_182;

const NS_PER_SEC: u64 = 1_000_000_000;

/// After this many channel-0 transitions in one catch-up (and at least one
/// rising edge), the rest of the backlog is skipped. See [`Pit::advance`].
const MAX_CATCHUP_TRANSITIONS: u32 = 8;

/// Access modes (control word bits 5:4) and read/write byte sequencing;
/// 1 (LSB only) and 0 (never programmed) both fall to the default arms.
const RW_MSB: u8 = 2;
const RW_WORD0: u8 = 3;
const RW_WORD1: u8 = 4;

/// QEMU `muldiv64`: `a * b / c` with a 128-bit intermediate.
fn muldiv64(a: u64, b: u64, c: u64) -> u64 {
    (a as u128 * b as u128 / c as u128) as u64
}

#[derive(Clone, Debug)]
struct Channel {
    /// Reload value, 1..=0x10000.
    count: u32,
    latched_count: u16,
    /// Pending counter latch: 0 = none, otherwise the RW_* state of the next byte.
    count_latched: u8,
    status_latched: bool,
    status: u8,
    read_state: u8,
    write_state: u8,
    write_latch: u8,
    rw_mode: u8,
    mode: u8,
    bcd: bool,
    gate: bool,
    load_time: Nanos,
}

impl Channel {
    fn new(gate: bool) -> Self {
        Channel {
            count: 0x10000,
            latched_count: 0,
            count_latched: 0,
            status_latched: false,
            status: 0,
            read_state: 0,
            write_state: 0,
            write_latch: 0,
            rw_mode: 0,
            mode: 3,
            bcd: false,
            gate,
            load_time: 0,
        }
    }

    fn ticks(&self, now: Nanos) -> u64 {
        muldiv64(now.saturating_sub(self.load_time), PIT_FREQ, NS_PER_SEC)
    }

    /// Current counter value (QEMU `pit_get_count`); 0x10000 reads as 0.
    fn counter(&self, now: Nanos) -> u32 {
        let d = self.ticks(now);
        let c = self.count as u64;
        let v = match self.mode {
            0 | 1 | 4 | 5 => c.wrapping_sub(d) & 0xFFFF,
            3 => c - d.wrapping_mul(2) % c,
            _ => c - d % c,
        };
        v as u32
    }

    /// OUT pin level (QEMU `pit_get_out`).
    fn out(&self, now: Nanos) -> bool {
        let d = self.ticks(now);
        let c = self.count as u64;
        match self.mode {
            1 => d < c,
            2 => d.is_multiple_of(c) && d != 0,
            3 => d % c < (c + 1) >> 1,
            4 | 5 => d == c,
            _ => d >= c,
        }
    }

    /// When QEMU's IRQ timer would next fire (QEMU
    /// `pit_get_next_transition_time`). Because tick times are rounded down
    /// to whole nanoseconds, the level evaluated at that instant is the one
    /// of the tick before; this is how QEMU's edges come out, so it is kept.
    fn next_transition(&self, now: Nanos) -> Option<Nanos> {
        let d = self.ticks(now);
        let c = self.count as u64;
        let next = match self.mode {
            2 => {
                let base = d / c * c;
                if d == base && d != 0 {
                    base + c
                } else {
                    base + c + 1
                }
            }
            3 => {
                let base = d / c * c;
                let half = (c + 1) >> 1;
                if d - base < half {
                    base + half
                } else {
                    base + c
                }
            }
            4 | 5 if d < c => c,
            4 | 5 if d == c => c + 1,
            4 | 5 => return None,
            _ if d < c => c,
            _ => return None,
        };
        let t = self
            .load_time
            .saturating_add(muldiv64(next, NS_PER_SEC, PIT_FREQ));
        Some(if t <= now { now + 1 } else { t })
    }

    fn load(&mut self, val: u32, now: Nanos) {
        self.count = if val == 0 { 0x10000 } else { val };
        self.load_time = now;
    }

    /// Byte written to the data port; returns a complete count to load.
    fn write_data(&mut self, val: u8) -> Option<u32> {
        match self.write_state {
            RW_MSB => Some((val as u32) << 8),
            RW_WORD0 => {
                self.write_latch = val;
                self.write_state = RW_WORD1;
                None
            }
            RW_WORD1 => {
                self.write_state = RW_WORD0;
                Some(self.write_latch as u32 | (val as u32) << 8)
            }
            _ => Some(val as u32),
        }
    }

    /// Counter latch command; ignored while an earlier latch is unread.
    fn latch_count(&mut self, now: Nanos) {
        if self.count_latched == 0 {
            self.latched_count = self.counter(now) as u16;
            self.count_latched = self.rw_mode;
        }
    }

    fn latch_status(&mut self, now: Nanos) {
        if !self.status_latched {
            self.status = (self.out(now) as u8) << 7
                | self.rw_mode << 4
                | self.mode << 1
                | self.bcd as u8;
            self.status_latched = true;
        }
    }

    fn read_data(&mut self, now: Nanos) -> u8 {
        if self.status_latched {
            self.status_latched = false;
            return self.status;
        }
        if self.count_latched != 0 {
            let [lo, hi] = self.latched_count.to_le_bytes();
            return match self.count_latched {
                RW_MSB => {
                    self.count_latched = 0;
                    hi
                }
                RW_WORD0 => {
                    self.count_latched = RW_MSB;
                    lo
                }
                _ => {
                    self.count_latched = 0;
                    lo
                }
            };
        }
        let [lo, hi, ..] = self.counter(now).to_le_bytes();
        match self.read_state {
            RW_MSB => hi,
            RW_WORD0 => {
                self.read_state = RW_WORD1;
                lo
            }
            RW_WORD1 => {
                self.read_state = RW_WORD0;
                hi
            }
            _ => lo,
        }
    }

    /// Gate input change (QEMU `pit_set_channel_gate`). Returns true when a
    /// rising edge restarted the count.
    fn set_gate(&mut self, gate: bool, now: Nanos) -> bool {
        let restart = matches!(self.mode, 1 | 2 | 3 | 5) && !self.gate && gate;
        if restart {
            self.load_time = now;
        }
        self.gate = gate;
        restart
    }
}

/// The 8254 and port 0x61.
#[derive(Clone, Debug)]
pub struct Pit {
    channels: [Channel; 3],
    /// Channel 0's IRQ timer (QEMU `next_transition_time`), None when idle.
    ch0_next: Option<Nanos>,
    /// IRQ0 line level as last queued for the PIC.
    irq_level: bool,
    /// IRQ0 level changes not yet handed to [`Pit::advance`]'s callback,
    /// with the time they happened.
    pending: Vec<(Nanos, bool)>,
    speaker_data: bool,
    refresh: bool,
}

impl Default for Pit {
    fn default() -> Self {
        Self::new()
    }
}

impl Pit {
    /// QEMU reset state: all channels mode 3 with count 0x10000 loaded at
    /// t = 0, gates high except channel 2's. The IRQ0 line starts low and
    /// channel 0's first timer event is armed.
    pub fn new() -> Self {
        let channels = [Channel::new(true), Channel::new(true), Channel::new(false)];
        let ch0_next = channels[0].next_transition(0);
        Pit {
            channels,
            ch0_next,
            irq_level: false,
            pending: Vec::new(),
            speaker_data: false,
            refresh: false,
        }
    }

    /// Guest read of 0x40-0x43. Latched status comes first, then a latched
    /// count, otherwise the live count in the channel's LSB/MSB/word order.
    /// 0x43 is write-only and reads 0.
    pub fn read(&mut self, port: u16, now: Nanos) -> u8 {
        match port {
            0x40..=0x42 => self.channels[(port - 0x40) as usize].read_data(now),
            0x43 => 0,
            _ => {
                unhandled("pit io read", port as u64, 1, None);
                0xFF
            }
        }
    }

    /// Guest write of 0x40-0x43: counter loads, control words, counter
    /// latch (access 0) and read-back (0xC0-0xFF).
    pub fn write(&mut self, port: u16, val: u8, now: Nanos) {
        match port {
            0x40..=0x42 => {
                let i = (port - 0x40) as usize;
                if let Some(count) = self.channels[i].write_data(val) {
                    if i == 0 {
                        // Edges already due come before the reload.
                        self.run_timer(now);
                    }
                    self.channels[i].load(count, now);
                    if i == 0 {
                        self.ch0_update(now);
                    }
                }
            }
            0x43 => self.write_control(val, now),
            _ => unhandled("pit io write", port as u64, 1, Some(val as u64)),
        }
    }

    fn write_control(&mut self, val: u8, now: Nanos) {
        let sel = (val >> 6) as usize;
        if sel == 3 {
            for (i, ch) in self.channels.iter_mut().enumerate() {
                if val & (2 << i) == 0 {
                    continue;
                }
                if val & 0x20 == 0 {
                    ch.latch_count(now);
                }
                if val & 0x10 == 0 {
                    ch.latch_status(now);
                }
            }
            return;
        }
        let ch = &mut self.channels[sel];
        let access = (val >> 4) & 3;
        if access == 0 {
            ch.latch_count(now);
        } else {
            ch.rw_mode = access;
            ch.read_state = access;
            ch.write_state = access;
            ch.mode = (val >> 1) & 7;
            ch.bcd = val & 1 != 0;
        }
    }

    /// Advance channel 0 to `now`, calling `irq0(level)` for every IRQ0
    /// level change up to `now`, in order (including changes caused by
    /// earlier counter loads).
    ///
    /// Catch-up policy: if the caller fell far behind (host stall), the
    /// backlog is not replayed pulse by pulse. After
    /// `MAX_CATCHUP_TRANSITIONS` transitions, once at least one rising
    /// edge has been delivered in this call, channel 0 jumps straight to
    /// its state at `now`. Since the guest cannot have acknowledged
    /// anything in between, the PIC would have merged those edges into one
    /// IRR bit anyway, which is also what QEMU shows after a stall.
    pub fn advance(&mut self, now: Nanos, irq0: &mut dyn FnMut(bool)) {
        self.run_timer(now);
        for (_, level) in self.pending.drain(..) {
            irq0(level);
        }
    }

    /// When [`Pit::advance`] next has work: the time of a queued IRQ0
    /// change, else channel 0's next timer event, or None when channel 0
    /// has nothing left to do (e.g. a finished one-shot). Like QEMU's
    /// timer, an event may occasionally leave the level unchanged.
    pub fn next_event(&self) -> Option<Nanos> {
        self.pending.first().map(|&(t, _)| t).or(self.ch0_next)
    }

    /// Port 0x61 read, as QEMU `pcspk_io_read`: bit 0 = channel 2 gate,
    /// bit 1 = speaker data enable, bit 4 = refresh toggle (flips on every
    /// read), bit 5 = channel 2 OUT.
    pub fn read_port61(&mut self, now: Nanos) -> u8 {
        self.refresh = !self.refresh;
        let ch2 = &self.channels[2];
        ch2.gate as u8
            | (self.speaker_data as u8) << 1
            | (self.refresh as u8) << 4
            | (ch2.out(now) as u8) << 5
    }

    /// Port 0x61 write: bit 0 = channel 2 gate, bit 1 = speaker data.
    /// Other bits are ignored, as in QEMU.
    pub fn write_port61(&mut self, val: u8, now: Nanos) {
        self.speaker_data = val & 2 != 0;
        self.channels[2].set_gate(val & 1 != 0, now);
    }

    /// Tone the PC speaker is producing: channel 2 gated on, speaker data
    /// on and channel 2 in mode 3. None when silent.
    pub fn speaker_hz(&self) -> Option<f64> {
        let ch2 = &self.channels[2];
        (ch2.gate && self.speaker_data && ch2.mode == 3)
            .then(|| PIT_FREQ as f64 / ch2.count as f64)
    }

    /// QEMU `pit_irq_timer_update` for channel 0 at time `t`.
    fn ch0_update(&mut self, t: Nanos) {
        let level = self.channels[0].out(t);
        if level != self.irq_level {
            self.irq_level = level;
            self.pending.push((t, level));
        }
        self.ch0_next = self.channels[0].next_transition(t);
    }

    /// Fire channel 0's timer events due by `now`, subject to the catch-up
    /// policy described on [`Pit::advance`].
    fn run_timer(&mut self, now: Nanos) {
        let mut processed = 0;
        let mut rose = false;
        while let Some(t) = self.ch0_next {
            if t > now {
                break;
            }
            if processed >= MAX_CATCHUP_TRANSITIONS
                && (rose || processed >= 4 * MAX_CATCHUP_TRANSITIONS)
            {
                self.ch0_update(now);
                break;
            }
            let was = self.irq_level;
            self.ch0_update(t);
            rose |= !was && self.irq_level;
            processed += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Time of PIT tick `n` after `t0`, rounded up to a whole nanosecond.
    fn tick_time(n: u64) -> Nanos {
        (n as u128 * NS_PER_SEC as u128).div_ceil(PIT_FREQ as u128) as Nanos
    }

    fn ticks_at(t: Nanos) -> u64 {
        muldiv64(t, PIT_FREQ, NS_PER_SEC)
    }

    fn templeos_ch0(pit: &mut Pit, now: Nanos) {
        pit.write(0x43, 0x34, now);
        pit.write(0x40, 0xA8, now);
        pit.write(0x40, 0x04, now);
    }

    fn read_ch0(pit: &mut Pit, now: Nanos) -> u16 {
        pit.write(0x43, 0x00, now);
        let lo = pit.read(0x40, now) as u16;
        let hi = pit.read(0x40, now) as u16;
        lo | hi << 8
    }

    #[test]
    fn reset_state() {
        let mut pit = Pit::new();
        pit.write(0x43, 0xE2, 0); // read-back status ch0
        assert_eq!(pit.read(0x40, 0), 0x86, "OUT high, mode 3, rw 0");
        assert_eq!(read_ch0(&mut pit, 0), 0, "count 0x10000 reads as 0");
        assert_eq!(pit.read_port61(0) & 1, 0, "ch2 gate low");
        // QEMU never drives IRQ0 at reset; the first timer event raises it.
        assert_eq!(pit.next_event(), Some(muldiv64(0x8000, NS_PER_SEC, PIT_FREQ)));
    }

    #[test]
    fn templeos_rate_generator_edges() {
        let mut pit = Pit::new();
        templeos_ch0(&mut pit, 0);
        // Coarse polling: levels strictly alternate, starting with a rise,
        // one pulse per 1192 ticks (~1.0 ms).
        let mut levels = Vec::new();
        let mut t = 0;
        while t < 20_000_000 {
            t += 50_000;
            pit.advance(t, &mut |l| levels.push(l));
        }
        let rise_count = levels.iter().filter(|&&l| l).count();
        assert_eq!(rise_count, (ticks_at(20_000_000) / 1192) as usize);
        for (i, &l) in levels.iter().enumerate() {
            assert_eq!(l, i % 2 == 0);
        }

        // Exact edge times, stepping event by event.
        let mut pit = Pit::new();
        templeos_ch0(&mut pit, 0);
        let mut rises = Vec::new();
        while rises.len() < 10 {
            let t = pit.next_event().unwrap();
            pit.advance(t, &mut |l| {
                if l {
                    rises.push(t)
                }
            });
        }
        for (k, &t) in rises.iter().enumerate() {
            assert_eq!(ticks_at(t), 1192 * (k as u64 + 1), "rise {k} at t={t}");
        }
    }

    #[test]
    fn latch_reads_decrease() {
        let mut pit = Pit::new();
        templeos_ch0(&mut pit, 0);
        let a = read_ch0(&mut pit, tick_time(100));
        let b = read_ch0(&mut pit, tick_time(500));
        assert_eq!(a, 1192 - 100);
        assert_eq!(b, 1192 - 500);
        assert_eq!(read_ch0(&mut pit, tick_time(1192)), 1192, "wrap reads the reload value");
        assert_eq!(read_ch0(&mut pit, tick_time(1193)), 1191);

        // A second latch before reading is ignored.
        pit.write(0x43, 0x00, tick_time(10));
        pit.write(0x43, 0x00, tick_time(20));
        let lo = pit.read(0x40, tick_time(30)) as u16;
        let hi = pit.read(0x40, tick_time(30)) as u16;
        assert_eq!(lo | hi << 8, 1192 - 10);
    }

    #[test]
    fn next_event_after_load() {
        let mut pit = Pit::new();
        let t0 = 1_000_000;
        templeos_ch0(&mut pit, t0);
        let mut seen = Vec::new();
        pit.advance(t0, &mut |l| seen.push(l));
        assert!(seen.is_empty(), "line was low and mode 2 starts low");
        let first = t0 + muldiv64(1193, NS_PER_SEC, PIT_FREQ);
        assert_eq!(pit.next_event(), Some(first));
        pit.advance(first - 1, &mut |l| seen.push(l));
        assert!(seen.is_empty());
        pit.advance(first, &mut |l| seen.push(l));
        assert_eq!(seen, [true]);
    }

    #[test]
    fn stall_catch_up_is_bounded() {
        let mut pit = Pit::new();
        templeos_ch0(&mut pit, 0);
        let mut seen = Vec::new();
        pit.advance(5 * NS_PER_SEC, &mut |l| seen.push(l));
        assert!(seen.len() <= MAX_CATCHUP_TRANSITIONS as usize + 2, "{seen:?}");
        assert!(seen.windows(2).any(|w| w == [true, false] || w == [false, true]));
        assert!(seen.contains(&true));
        assert!(pit.next_event().unwrap() > 5 * NS_PER_SEC);
    }

    #[test]
    fn one_shot_finishes() {
        let mut pit = Pit::new();
        pit.write(0x43, 0x30, 0); // ch0 mode 0, word
        pit.write(0x40, 100, 0);
        pit.write(0x40, 0, 0);
        let mut seen = Vec::new();
        pit.advance(tick_time(99), &mut |l| seen.push(l));
        assert!(seen.is_empty());
        pit.advance(tick_time(101), &mut |l| seen.push(l));
        assert_eq!(seen, [true]);
        assert_eq!(pit.next_event(), None);
    }

    #[test]
    fn readback_status_and_count() {
        let mut pit = Pit::new();
        templeos_ch0(&mut pit, 0);
        pit.write(0x43, 0xC2, tick_time(200)); // latch count + status of ch0
        assert_eq!(pit.read(0x40, tick_time(300)), 0x34, "OUT low, word, mode 2");
        assert_eq!(pit.read(0x40, tick_time(300)), ((1192 - 200) & 0xFF) as u8);
        assert_eq!(pit.read(0x40, tick_time(300)), ((1192 - 200) >> 8) as u8);
        // Unlatched reads follow the live count.
        let lo = pit.read(0x40, tick_time(400)) as u16;
        let hi = pit.read(0x40, tick_time(400)) as u16;
        assert_eq!(lo | hi << 8, 1192 - 400);
    }

    #[test]
    fn port61_refresh_toggles() {
        let mut pit = Pit::new();
        let a = pit.read_port61(0) & 0x10;
        let b = pit.read_port61(0) & 0x10;
        let c = pit.read_port61(0) & 0x10;
        assert_eq!(a, 0x10);
        assert_eq!(b, 0);
        assert_eq!(c, 0x10);
    }

    #[test]
    fn port61_out2_square_wave() {
        let mut pit = Pit::new();
        pit.write(0x43, 0xB6, 0);
        pit.write(0x42, 0x00, 0);
        pit.write(0x42, 0x01, 0); // 256
        let t0 = 10_000_000;
        pit.write_port61(0x03, t0); // gate rising edge restarts the count
        assert_eq!(pit.read_port61(t0) & 0x23, 0x23);
        assert_eq!(pit.read_port61(t0 + tick_time(127)) & 0x20, 0x20);
        assert_eq!(pit.read_port61(t0 + tick_time(128)) & 0x20, 0);
        assert_eq!(pit.read_port61(t0 + tick_time(255)) & 0x20, 0);
        assert_eq!(pit.read_port61(t0 + tick_time(256)) & 0x20, 0x20);
        let hz = pit.speaker_hz().unwrap();
        assert!((hz - PIT_FREQ as f64 / 256.0).abs() < 1e-9);
        pit.write_port61(0x01, t0);
        assert_eq!(pit.speaker_hz(), None);
    }

    #[test]
    fn port61_out2_mode0() {
        // SeaBIOS-style delay: ch2 mode 0, poll OUT via port 0x61.
        let mut pit = Pit::new();
        pit.write_port61(0x01, 0);
        pit.write(0x43, 0xB0, 0);
        pit.write(0x42, 50, 0);
        pit.write(0x42, 0, 0);
        assert_eq!(pit.read_port61(tick_time(49)) & 0x20, 0);
        assert_eq!(pit.read_port61(tick_time(50)) & 0x20, 0x20);
        assert_eq!(pit.read_port61(tick_time(5000)) & 0x20, 0x20);
    }
}
