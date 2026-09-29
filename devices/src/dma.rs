//! The two i8257 (8237) DMA controllers and their page registers, as QEMU
//! 8.2 `hw/dma/i8257.c` presents them:
//!
//! - Controller 1 (channels 0-3, byte transfers): channel registers at
//!   0x00-0x07, control registers at 0x08-0x0F, page registers at
//!   0x81-0x83 and 0x87.
//! - Controller 2 (channels 4-7, word transfers): registers at 0xC0-0xDF on
//!   even ports; QEMU decodes only address bits 4:1, so odd ports alias the
//!   even port below them. Page registers at 0x89-0x8B and 0x8F.
//!
//! Only register state is modelled: nothing on this machine does ISA DMA,
//! so no transfers ever run and the current count stays at its initial
//! value. (QEMU's high page registers at 0x481-0x48F are not decoded here.)

use crate::unhandled;

/// Page register offset (port & 7) to channel (QEMU `channels[]`).
const PAGE_CHANNEL: [Option<usize>; 8] =
    [None, Some(2), Some(3), Some(1), None, None, None, Some(0)];

/// Command register bits QEMU rejects (everything but controller disable).
const CMD_NOT_SUPPORTED: u8 = 0xfb;

#[derive(Clone, Copy, Default)]
struct Channel {
    /// Base address and base count, as written.
    base: [u16; 2],
    /// Current address (in bytes, i.e. shifted for controller 2) and count
    /// transferred, reloaded when the high byte of either base register
    /// is written.
    now_addr: u32,
    now_count: u32,
    mode: u8,
    page: u8,
}

struct Controller {
    /// 0 for controller 1, 1 for controller 2 (word addressing).
    dshift: u32,
    status: u8,
    command: u8,
    mask: u8,
    flip_flop: bool,
    regs: [Channel; 4],
}

impl Controller {
    fn new(dshift: u32) -> Self {
        let mut c = Self {
            dshift,
            status: 0,
            command: 0,
            mask: 0,
            flip_flop: false,
            regs: [Channel::default(); 4],
        };
        c.master_clear();
        c
    }

    fn master_clear(&mut self) {
        self.flip_flop = false;
        self.mask = 0xff;
        self.status = 0;
        self.command = 0;
    }

    fn getff(&mut self) -> bool {
        let ff = self.flip_flop;
        self.flip_flop = !ff;
        ff
    }

    /// Channel register `iport` (0-7): even = address, odd = count.
    fn read_chan(&mut self, iport: usize) -> u8 {
        let ichan = iport >> 1;
        let ff = self.getff();
        let r = &self.regs[ichan];
        let val = if iport & 1 == 1 {
            (u32::from(r.base[1]) << self.dshift).wrapping_sub(r.now_count)
        } else if (r.mode >> 5) & 1 == 1 {
            r.now_addr.wrapping_sub(r.now_count)
        } else {
            r.now_addr.wrapping_add(r.now_count)
        };
        (val >> (self.dshift + if ff { 8 } else { 0 })) as u8
    }

    fn write_chan(&mut self, iport: usize, data: u8) {
        let ichan = iport >> 1;
        let nreg = iport & 1;
        let dshift = self.dshift;
        let high = self.getff();
        let r = &mut self.regs[ichan];
        if high {
            r.base[nreg] = (r.base[nreg] & 0x00ff) | (u16::from(data) << 8);
            r.now_addr = u32::from(r.base[0]) << dshift;
            r.now_count = 0;
        } else {
            r.base[nreg] = (r.base[nreg] & 0xff00) | u16::from(data);
        }
    }

    /// Control register `iport` (0-7, i.e. port 0x08-0x0F or 0xD0-0xDE).
    fn read_cont(&mut self, iport: usize) -> u8 {
        match iport {
            0 => {
                let v = self.status;
                self.status &= 0xf0;
                v
            }
            // QEMU returns the mask register here.
            1 => self.mask,
            _ => 0,
        }
    }

    fn write_cont(&mut self, iport: usize, data: u8) {
        match iport {
            0 => {
                if data != 0 && data & CMD_NOT_SUPPORTED != 0 {
                    unhandled("dma command", u64::from(data), 1, None);
                    return;
                }
                self.command = data;
            }
            1 => {
                // Software DMA request.
                let ichan = data & 3;
                if data & 4 != 0 {
                    self.status |= 1 << (ichan + 4);
                } else {
                    self.status &= !(1 << (ichan + 4));
                }
                self.status &= !(1 << ichan);
            }
            2 => {
                if data & 4 != 0 {
                    self.mask |= 1 << (data & 3);
                } else {
                    self.mask &= !(1 << (data & 3));
                }
            }
            3 => self.regs[(data & 3) as usize].mode = data,
            4 => self.flip_flop = false,
            5 => self.master_clear(),
            6 => self.mask = 0,
            7 => self.mask = data,
            _ => unreachable!(),
        }
    }
}

/// Both DMA controllers.
pub struct Dma {
    ctrl: [Controller; 2],
}

impl Default for Dma {
    fn default() -> Self {
        Self::new()
    }
}

impl Dma {
    /// Both controllers after reset: all channels masked (mask reads 0xFF),
    /// flip-flops clear, all address/count/page registers zero.
    pub fn new() -> Self {
        Self { ctrl: [Controller::new(0), Controller::new(1)] }
    }

    /// True for the ports QEMU's i8257 devices decode.
    pub fn handles(port: u16) -> bool {
        matches!(port, 0x00..=0x0f | 0xc0..=0xdf) || Self::page_channel(port).is_some()
    }

    /// (controller, channel) for a page register port.
    fn page_channel(port: u16) -> Option<(usize, usize)> {
        let ctrl = match port {
            0x80..=0x87 => 0,
            0x88..=0x8f => 1,
            _ => return None,
        };
        PAGE_CHANNEL[usize::from(port & 7)].map(|ch| (ctrl, ch))
    }

    /// Split a controller port into (controller, register 0-15).
    fn decode(port: u16) -> Option<(usize, usize)> {
        match port {
            0x00..=0x0f => Some((0, usize::from(port))),
            0xc0..=0xdf => Some((1, usize::from((port - 0xc0) >> 1) & 0x0f)),
            _ => None,
        }
    }

    /// Register read. Channel address/count registers return the low byte
    /// then the high byte, toggled by the shared flip-flop.
    pub fn read(&mut self, port: u16) -> u8 {
        if let Some((c, ch)) = Self::page_channel(port) {
            return self.ctrl[c].regs[ch].page;
        }
        match Self::decode(port) {
            Some((c, reg)) if reg < 8 => self.ctrl[c].read_chan(reg),
            Some((c, reg)) => self.ctrl[c].read_cont(reg - 8),
            None => {
                unhandled("dma read", port.into(), 1, None);
                0xff
            }
        }
    }

    /// Register write. No transfer is ever started.
    pub fn write(&mut self, port: u16, val: u8) {
        if let Some((c, ch)) = Self::page_channel(port) {
            self.ctrl[c].regs[ch].page = val;
            return;
        }
        match Self::decode(port) {
            Some((c, reg)) if reg < 8 => self.ctrl[c].write_chan(reg, val),
            Some((c, reg)) => self.ctrl[c].write_cont(reg - 8, val),
            None => unhandled("dma write", port.into(), 1, Some(val.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        assert!(Dma::handles(0x00) && Dma::handles(0x0f));
        assert!(Dma::handles(0xc0) && Dma::handles(0xdf) && Dma::handles(0xd5));
        assert!(!Dma::handles(0x80));
        assert!(Dma::handles(0x81) && Dma::handles(0x87) && Dma::handles(0x8f));
        assert!(!Dma::handles(0x84) && !Dma::handles(0x88));
        assert!(!Dma::handles(0x10) && !Dma::handles(0xe0));
    }

    #[test]
    fn address_flip_flop_readback() {
        let mut d = Dma::new();
        d.write(0x0c, 0); // clear flip-flop
        d.write(0x04, 0x34); // channel 2 address low
        d.write(0x04, 0x12); // high: reloads the current address
        d.write(0x05, 0xff);
        d.write(0x05, 0x01); // count 0x1ff
        d.write(0x0c, 0);
        assert_eq!(d.read(0x04), 0x34);
        assert_eq!(d.read(0x04), 0x12);
        assert_eq!(d.read(0x05), 0xff);
        assert_eq!(d.read(0x05), 0x01);

        // Current address only reloads on a high-byte write.
        d.write(0x0c, 0);
        d.write(0x04, 0x78);
        d.write(0x0c, 0);
        assert_eq!(d.read(0x04), 0x34);

        // The flip-flop is shared by reads and writes.
        d.write(0x0c, 0);
        d.write(0x04, 0x00);
        assert_eq!(d.read(0x04), 0x12);
    }

    #[test]
    fn controller2_word_registers() {
        let mut d = Dma::new();
        d.write(0xd8, 0); // clear flip-flop
        d.write(0xc4, 0x34); // channel 5 address
        d.write(0xc4, 0x12);
        d.write(0xd8, 0);
        assert_eq!(d.read(0xc4), 0x34);
        assert_eq!(d.read(0xc5), 0x12, "odd port aliases the even one");
    }

    #[test]
    fn control_registers() {
        let mut d = Dma::new();
        // SeaBIOS POST sequence.
        d.write(0x0d, 0);
        d.write(0xda, 0);
        d.write(0xd6, 0xc0);
        d.write(0xd4, 0);
        assert_eq!(d.read(0x09), 0xff, "mask after master clear");
        assert_eq!(d.read(0xd2), 0xfe, "channel 4 unmasked");
        d.write(0x0f, 0x05);
        assert_eq!(d.read(0x09), 0x05);
        d.write(0x0e, 0);
        assert_eq!(d.read(0x09), 0x00);
        // Software request shows in status; reading clears the low nibble only.
        d.write(0x09, 0x06);
        assert_eq!(d.read(0x08), 0x40);
        assert_eq!(d.read(0x08), 0x40);
        assert_eq!(d.read(0x0d), 0);
    }

    #[test]
    fn page_registers() {
        let mut d = Dma::new();
        for (port, val) in [(0x81, 1), (0x82, 2), (0x83, 3), (0x87, 4), (0x89, 5), (0x8a, 6), (0x8b, 7), (0x8f, 8)] {
            d.write(port, val);
        }
        for (port, val) in [(0x81, 1), (0x82, 2), (0x83, 3), (0x87, 4), (0x89, 5), (0x8a, 6), (0x8b, 7), (0x8f, 8)] {
            assert_eq!(d.read(port), val);
        }
        assert_eq!(d.ctrl[0].regs[2].page, 1);
        assert_eq!(d.ctrl[0].regs[0].page, 4);
        assert_eq!(d.ctrl[1].regs[3].page, 6);
        assert_eq!(d.ctrl[1].regs[1].page, 7);
    }
}
