//! Cascaded 8259A PICs (master 0x20/0x21, slave 0xA0/0xA1 on master IRQ2)
//! plus the PIIX3 edge/level control registers (ELCR, 0x4D0/0x4D1).
//!
//! This is a direct model of QEMU's `hw/intc/i8259.c`: the same IRR edge
//! detection (`last_irr`), priority resolution, OCW handling and INTA
//! behavior, so IRR/ISR reads and vector order match the reference machine.
//! ICW4 buffered-mode and 8086-mode bits are accepted and ignored, as QEMU
//! does.

use crate::unhandled;

/// One 8259A.
#[derive(Clone, Debug)]
struct Chip {
    /// Wired as master (its IRQ2 is the cascade input).
    master: bool,
    /// Input levels last seen, for edge detection.
    last_irr: u8,
    irr: u8,
    imr: u8,
    isr: u8,
    /// Rotation: the IRQ with the highest priority is `priority_add`.
    priority_add: u8,
    irq_base: u8,
    /// OCW3 read-register select: false = IRR (reset default), true = ISR.
    read_isr: bool,
    /// OCW3 poll command pending: the next read is a poll.
    poll: bool,
    special_mask: bool,
    /// 0 = operational, 1..=3 = expecting ICW2..ICW4.
    init_state: u8,
    init4: bool,
    single_mode: bool,
    auto_eoi: bool,
    rotate_on_auto_eoi: bool,
    special_fully_nested: bool,
    /// ICW1 LTIM: every input level triggered.
    ltim: bool,
    elcr: u8,
    elcr_mask: u8,
}

impl Chip {
    fn new(master: bool, elcr_mask: u8) -> Self {
        Chip {
            master,
            last_irr: 0,
            irr: 0,
            imr: 0,
            isr: 0,
            priority_add: 0,
            irq_base: 0,
            read_isr: false,
            poll: false,
            special_mask: false,
            init_state: 0,
            init4: false,
            single_mode: false,
            auto_eoi: false,
            rotate_on_auto_eoi: false,
            special_fully_nested: false,
            ltim: false,
            elcr: 0,
            elcr_mask,
        }
    }

    /// QEMU `pic_reset_common`: everything but the ELCR; level-triggered
    /// requests survive in IRR.
    fn init_reset(&mut self) {
        let (master, elcr, elcr_mask, irr) = (self.master, self.elcr, self.elcr_mask, self.irr);
        *self = Chip::new(master, elcr_mask);
        self.elcr = elcr;
        self.irr = irr & elcr;
    }

    fn level_triggered(&self, irq: u8) -> bool {
        self.ltim || self.elcr & (1 << irq) != 0
    }

    /// Highest priority (0 = highest) present in `mask`, or 8 if none.
    fn priority(&self, mask: u8) -> u8 {
        if mask == 0 {
            return 8;
        }
        let mut p = 0;
        while mask & (1 << ((p + self.priority_add) & 7)) == 0 {
            p += 1;
        }
        p
    }

    /// The IRQ this chip would present on INT right now (QEMU `pic_get_irq`).
    fn pending(&self) -> Option<u8> {
        let p = self.priority(self.irr & !self.imr);
        if p == 8 {
            return None;
        }
        let mut in_service = self.isr;
        if self.special_mask {
            in_service &= !self.imr;
        }
        if self.special_fully_nested && self.master {
            in_service &= !(1 << 2);
        }
        (p < self.priority(in_service)).then_some((p + self.priority_add) & 7)
    }

    fn set_irq(&mut self, irq: u8, level: bool) {
        let mask = 1 << irq;
        if self.level_triggered(irq) {
            if level {
                self.irr |= mask;
                self.last_irr |= mask;
            } else {
                self.irr &= !mask;
                self.last_irr &= !mask;
            }
        } else if level {
            if self.last_irr & mask == 0 {
                self.irr |= mask;
            }
            self.last_irr |= mask;
        } else {
            self.last_irr &= !mask;
        }
    }

    /// INTA for `irq`. Level-triggered requests stay in IRR.
    fn intack(&mut self, irq: u8) {
        if self.auto_eoi {
            if self.rotate_on_auto_eoi {
                self.priority_add = (irq + 1) & 7;
            }
        } else {
            self.isr |= 1 << irq;
        }
        if !self.level_triggered(irq) {
            self.irr &= !(1 << irq);
        }
    }

    fn write(&mut self, a0: bool, val: u8) {
        if !a0 {
            if val & 0x10 != 0 {
                // ICW1
                self.init_reset();
                self.init_state = 1;
                self.init4 = val & 0x01 != 0;
                self.single_mode = val & 0x02 != 0;
                self.ltim = val & 0x08 != 0;
            } else if val & 0x08 != 0 {
                // OCW3
                if val & 0x04 != 0 {
                    self.poll = true;
                }
                if val & 0x02 != 0 {
                    self.read_isr = val & 0x01 != 0;
                }
                if val & 0x40 != 0 {
                    self.special_mask = val & 0x20 != 0;
                }
            } else {
                // OCW2
                match val >> 5 {
                    0 | 4 => self.rotate_on_auto_eoi = val >> 7 != 0,
                    cmd @ (1 | 5) => {
                        let p = self.priority(self.isr);
                        if p != 8 {
                            let irq = (p + self.priority_add) & 7;
                            self.isr &= !(1 << irq);
                            if cmd == 5 {
                                self.priority_add = (irq + 1) & 7;
                            }
                        }
                    }
                    3 => self.isr &= !(1 << (val & 7)),
                    6 => self.priority_add = (val + 1) & 7,
                    7 => {
                        let irq = val & 7;
                        self.isr &= !(1 << irq);
                        self.priority_add = (irq + 1) & 7;
                    }
                    _ => {} // 2: no operation
                }
            }
        } else {
            match self.init_state {
                0 => self.imr = val,
                1 => {
                    self.irq_base = val & 0xF8;
                    self.init_state = match (self.single_mode, self.init4) {
                        (false, _) => 2,
                        (true, true) => 3,
                        (true, false) => 0,
                    };
                }
                2 => self.init_state = if self.init4 { 3 } else { 0 },
                _ => {
                    // ICW4: only SFNM and AEOI matter; µPM, M/S and BUF are ignored.
                    self.special_fully_nested = val & 0x10 != 0;
                    self.auto_eoi = val & 0x02 != 0;
                    self.init_state = 0;
                }
            }
        }
    }

    fn read(&mut self, a0: bool) -> u8 {
        if self.poll {
            self.poll = false;
            match self.pending() {
                Some(irq) => {
                    self.intack(irq);
                    0x80 | irq
                }
                None => 0,
            }
        } else if a0 {
            self.imr
        } else if self.read_isr {
            self.isr
        } else {
            self.irr
        }
    }
}

/// The master/slave 8259A pair of a PIIX3 PC.
#[derive(Clone, Debug)]
pub struct Pic {
    master: Chip,
    slave: Chip,
}

impl Default for Pic {
    fn default() -> Self {
        Self::new()
    }
}

impl Pic {
    /// Power-on state as QEMU: both chips uninitialised (vector base 0,
    /// nothing masked), ELCR all edge. The ELCR write masks are PIIX3's:
    /// IRQ0-2 (master) and IRQ8/IRQ13 (slave) are always edge triggered.
    pub fn new() -> Self {
        Pic {
            master: Chip::new(true, 0xF8),
            slave: Chip::new(false, 0xDE),
        }
    }

    /// Drive ISA IRQ line `irq` (0..=15) to `level`. Edge-triggered inputs
    /// latch into IRR on a low-to-high transition only; level-triggered ones
    /// (per ELCR) follow the line.
    pub fn set_irq(&mut self, irq: u8, level: bool) {
        match irq {
            0..=7 => self.master.set_irq(irq, level),
            8..=15 => {
                self.slave.set_irq(irq - 8, level);
                self.cascade();
            }
            _ => unhandled("pic irq", irq as u64, 1, Some(level as u64)),
        }
    }

    /// Guest port read. Port 0x20/0xA0 returns IRR or ISR per the last
    /// OCW3 read select (IRR after init), or performs a poll if one was
    /// requested; 0x21/0xA1 return the IMR; 0x4D0/0x4D1 the ELCR.
    pub fn read(&mut self, port: u16) -> u8 {
        match port {
            0x20 | 0x21 => self.master.read(port & 1 != 0),
            0xA0 | 0xA1 => {
                let v = self.slave.read(port & 1 != 0);
                self.cascade();
                v
            }
            0x4D0 => self.master.elcr,
            0x4D1 => self.slave.elcr,
            _ => {
                unhandled("pic io read", port as u64, 1, None);
                0xFF
            }
        }
    }

    /// Guest port write: ICW1-4 / OCW1-3 on 0x20/0x21 and 0xA0/0xA1, ELCR
    /// on 0x4D0/0x4D1 (bits outside the PIIX3 mask stay 0).
    pub fn write(&mut self, port: u16, val: u8) {
        match port {
            0x20 | 0x21 => self.master.write(port & 1 != 0, val),
            0xA0 | 0xA1 => {
                self.slave.write(port & 1 != 0, val);
                self.cascade();
            }
            0x4D0 => self.master.elcr = val & self.master.elcr_mask,
            0x4D1 => self.slave.elcr = val & self.slave.elcr_mask,
            _ => unhandled("pic io write", port as u64, 1, Some(val as u64)),
        }
    }

    /// IRR/IMR/ISR of both chips, for debug output.
    pub fn debug_state(&self) -> String {
        let (m, s) = (&self.master, &self.slave);
        format!(
            "master irr={:02x} imr={:02x} isr={:02x} slave irr={:02x} imr={:02x} isr={:02x}",
            m.irr, m.imr, m.isr, s.irr, s.imr, s.isr
        )
    }

    /// Level of the master's INT output (the CPU's INTR / LINT0 ExtINT).
    pub fn has_interrupt(&self) -> bool {
        self.master.pending().is_some()
    }

    /// INTA cycle: returns the vector and updates IRR/ISR as QEMU's
    /// `pic_read_irq`. With nothing pending the master answers with its
    /// IRQ7 vector (spurious, ISR untouched); a cascade request whose slave
    /// source has gone answers with the slave's IRQ15 vector while the
    /// master still marks IRQ2 in service.
    pub fn acknowledge(&mut self) -> u8 {
        match self.master.pending() {
            Some(2) => {
                let vector = match self.slave.pending() {
                    Some(irq) => {
                        self.slave.intack(irq);
                        self.cascade();
                        self.slave.irq_base | irq
                    }
                    None => self.slave.irq_base | 7,
                };
                self.master.intack(2);
                vector
            }
            Some(irq) => {
                self.master.intack(irq);
                self.master.irq_base | irq
            }
            None => self.master.irq_base | 7,
        }
    }

    /// Propagate the slave's INT output to master IRQ2 (QEMU calls this on
    /// every slave update; repeating an unchanged level has no effect).
    fn cascade(&mut self) {
        let out = self.slave.pending().is_some();
        self.master.set_irq(2, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Kernel/KInts.HC: IntInit1`.
    fn templeos_init() -> Pic {
        let mut pic = Pic::new();
        for (port, val) in [
            (0x20, 0x11),
            (0xA0, 0x11),
            (0x21, 0x20),
            (0xA1, 0x28),
            (0x21, 0x04),
            (0xA1, 0x02),
            (0x21, 0x0D),
            (0xA1, 0x09),
            (0x21, 0xFA),
            (0xA1, 0xFF),
        ] {
            pic.write(port, val);
        }
        pic
    }

    #[test]
    fn templeos_init_sequence() {
        let mut pic = templeos_init();
        assert_eq!(pic.read(0x21), 0xFA);
        assert_eq!(pic.read(0xA1), 0xFF);
        assert_eq!(pic.read(0x20), 0, "IRR selected and empty");
        assert_eq!(pic.read(0xA0), 0);
        assert!(!pic.has_interrupt());
        assert!(!pic.master.auto_eoi && !pic.slave.auto_eoi);
        assert_eq!(pic.master.init_state, 0);
        assert_eq!(pic.slave.init_state, 0);
    }

    #[test]
    fn timer_irq_ack_and_eoi() {
        let mut pic = templeos_init();
        pic.set_irq(0, true);
        assert_eq!(pic.read(0x20) & 1, 1, "IRR bit 0 visible before ack");
        assert!(pic.has_interrupt());
        assert_eq!(pic.acknowledge(), 0x20);
        assert_eq!(pic.read(0x20) & 1, 0, "edge request cleared by INTA");
        pic.write(0x20, 0x0B); // OCW3: read ISR
        assert_eq!(pic.read(0x20), 0x01);
        pic.write(0x20, 0x0A); // back to IRR
        assert!(!pic.has_interrupt());

        // A new edge while in service latches but is held off by priority.
        pic.set_irq(0, false);
        pic.set_irq(0, true);
        assert_eq!(pic.read(0x20), 0x01);
        assert!(!pic.has_interrupt());

        pic.write(0x20, 0x20); // non-specific EOI
        pic.write(0x20, 0x0B);
        assert_eq!(pic.read(0x20), 0);
        assert!(pic.has_interrupt());
        assert_eq!(pic.acknowledge(), 0x20);
    }

    #[test]
    fn edge_needs_a_low_level_in_between() {
        let mut pic = templeos_init();
        pic.set_irq(0, true);
        assert_eq!(pic.acknowledge(), 0x20);
        pic.write(0x20, 0x20);
        pic.set_irq(0, true); // still high: no new edge
        assert_eq!(pic.read(0x20), 0);
        assert!(!pic.has_interrupt());
    }

    #[test]
    fn masked_irq_still_shows_in_irr() {
        let mut pic = templeos_init();
        pic.set_irq(1, true);
        assert_eq!(pic.read(0x20), 0x02);
        assert!(!pic.has_interrupt());
        pic.write(0x21, 0xF8);
        assert!(pic.has_interrupt());
        assert_eq!(pic.acknowledge(), 0x21);
    }

    #[test]
    fn cascade_irq12() {
        let mut pic = templeos_init();
        pic.write(0xA1, 0xEF);
        pic.set_irq(12, true);
        assert_eq!(pic.read(0xA0), 0x10);
        assert_eq!(pic.read(0x20), 0x04, "cascade shows as master IRQ2");
        assert!(pic.has_interrupt());
        assert_eq!(pic.acknowledge(), 0x2C);
        assert!(!pic.has_interrupt());
        pic.write(0xA0, 0x0B);
        pic.write(0x20, 0x0B);
        assert_eq!(pic.read(0xA0), 0x10);
        assert_eq!(pic.read(0x20), 0x04);
        pic.write(0xA0, 0x20);
        pic.write(0x20, 0x20);
        assert_eq!(pic.read(0xA0), 0);
        assert_eq!(pic.read(0x20), 0);
    }

    #[test]
    fn spurious_master_and_slave() {
        let mut pic = templeos_init();
        assert_eq!(pic.acknowledge(), 0x27);
        pic.write(0x20, 0x0B);
        assert_eq!(pic.read(0x20), 0, "spurious IRQ7 not put in service");

        pic.write(0xA1, 0xEF);
        pic.set_irq(12, true);
        pic.write(0xA1, 0xFF); // slave source disappears, master IRR2 stays latched
        assert!(pic.has_interrupt());
        assert_eq!(pic.acknowledge(), 0x2F);
        assert_eq!(pic.read(0x20), 0x04, "master IRQ2 in service");
    }

    #[test]
    fn elcr_masks_and_level_mode() {
        let mut pic = templeos_init();
        pic.write(0x4D0, 0xFF);
        pic.write(0x4D1, 0xFF);
        assert_eq!(pic.read(0x4D0), 0xF8);
        assert_eq!(pic.read(0x4D1), 0xDE);
        pic.write(0x4D0, 0x00);
        pic.write(0x4D1, 0x0C); // SeaBIOS: IRQ10/11 level
        assert_eq!(pic.read(0x4D1), 0x0C);

        pic.write(0xA1, 0xF7);
        pic.set_irq(11, true);
        assert_eq!(pic.acknowledge(), 0x2B);
        assert_eq!(pic.read(0xA0), 0x08, "level request stays in IRR");
        pic.write(0xA0, 0x20);
        pic.write(0x20, 0x20);
        assert!(pic.has_interrupt(), "still asserted after EOI");
        pic.set_irq(11, false);
        assert_eq!(pic.read(0xA0), 0, "level request follows the line");
        // Master IRQ2 is edge triggered and stays latched: the late INTA
        // finds nothing on the slave, as in QEMU.
        assert_eq!(pic.acknowledge(), 0x2F);
    }

    #[test]
    fn priority_and_specific_eoi() {
        let mut pic = templeos_init();
        pic.write(0x21, 0x00);
        pic.set_irq(3, true);
        pic.set_irq(1, true);
        assert_eq!(pic.acknowledge(), 0x21);
        pic.set_irq(0, true);
        assert_eq!(pic.acknowledge(), 0x20, "higher priority nests");
        assert!(!pic.has_interrupt(), "IRQ3 below in-service IRQ1");
        pic.write(0x20, 0x61); // specific EOI IRQ1
        pic.write(0x20, 0x0B);
        assert_eq!(pic.read(0x20), 0x01);
        assert!(!pic.has_interrupt());
        pic.write(0x20, 0x20);
        assert_eq!(pic.acknowledge(), 0x23);
    }

    #[test]
    fn auto_eoi_and_poll() {
        let mut pic = Pic::new();
        for (p, v) in [(0x20, 0x11), (0x21, 0x08), (0x21, 0x04), (0x21, 0x03), (0x21, 0x00)] {
            pic.write(p, v);
        }
        pic.set_irq(5, true);
        assert_eq!(pic.acknowledge(), 0x0D);
        pic.write(0x20, 0x0B);
        assert_eq!(pic.read(0x20), 0, "auto-EOI leaves ISR clear");

        pic.set_irq(4, true);
        pic.write(0x20, 0x0C); // poll
        assert_eq!(pic.read(0x20), 0x84);
        assert_eq!(pic.read(0x20), 0, "poll is one-shot; ISR select still active");
        pic.write(0x20, 0x0C);
        assert_eq!(pic.read(0x20), 0x00, "poll with nothing pending");
    }
}
