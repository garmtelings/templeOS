//! i8042 keyboard controller (ports 0x60/0x64) with a PS/2 keyboard on the
//! first port and a PS/2 (IntelliMouse-capable) mouse on the aux port.
//!
//! Behavior follows QEMU 8.2 `hw/input/pckbd.c` (with its default
//! `extended-state=on`, `kbd-throttle=off`) and `hw/input/ps2.c`:
//!
//! - The keyboard and mouse each have a 16-byte queue. The controller has a
//!   one-byte buffer of its own for command results (0x20, 0xAA, 0xD2, ...).
//!   When several sources have data, the output buffer is filled in the
//!   order controller-kbd, controller-aux, keyboard, mouse.
//! - Status bit 5 (AUX_OBF) is set whenever the byte in the output buffer
//!   came from the mouse or from command 0xD3.
//! - Keyboard command replies are queued in front of pending scan codes, and
//!   unread replies are discarded when the next command arrives.

use std::collections::VecDeque;

use crate::unhandled;

// Controller status register.
const STAT_OBF: u8 = 0x01;
const STAT_SELFTEST: u8 = 0x04;
const STAT_CMD: u8 = 0x08;
const STAT_UNLOCKED: u8 = 0x10;
const STAT_MOUSE_OBF: u8 = 0x20;

// Controller command byte ("mode").
const MODE_KBD_INT: u8 = 0x01;
const MODE_MOUSE_INT: u8 = 0x02;
const MODE_DISABLE_KBD: u8 = 0x10;
const MODE_DISABLE_MOUSE: u8 = 0x20;
const MODE_KCC: u8 = 0x40;

// Output port.
const OUT_RESET: u8 = 0x01;
const OUT_A20: u8 = 0x02;
const OUT_OBF: u8 = 0x10;
const OUT_MOUSE_OBF: u8 = 0x20;

// Sources with data waiting (QEMU `pending`). KBD/AUX share bit positions
// with the mode's disable bits so a disabled port masks its own data.
const PENDING_CTRL_KBD: u8 = 0x04;
const PENDING_CTRL_AUX: u8 = 0x08;
const PENDING_KBD: u8 = MODE_DISABLE_KBD;
const PENDING_AUX: u8 = MODE_DISABLE_MOUSE;

/// Where the byte in the output buffer comes from (QEMU `obsrc`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObSource {
    None,
    Kbd,
    Mouse,
    Ctrl,
}

// Controller commands that take a data byte on port 0x60.
const CCMD_WRITE_MODE: u8 = 0x60;
const CCMD_WRITE_OUTPORT: u8 = 0xd1;
const CCMD_WRITE_OBUF: u8 = 0xd2;
const CCMD_WRITE_AUX_OBUF: u8 = 0xd3;
const CCMD_WRITE_MOUSE: u8 = 0xd4;

const REPLY_ACK: u8 = 0xfa;
const REPLY_RESEND: u8 = 0xfe;
const REPLY_POR: u8 = 0xaa;
const REPLY_ID: u8 = 0xab;

/// Maximum bytes a PS/2 device buffers (QEMU `PS2_QUEUE_SIZE`).
const QUEUE_SIZE: usize = 16;

/// Scan code set 2 to set 1, as applied by the controller's translation
/// (QEMU `translate_table`).
static TRANSLATE: [u8; 256] = build_translate();

const fn build_translate() -> [u8; 256] {
    const HEAD: [u8; 0x88] = [
        0xff, 0x43, 0x41, 0x3f, 0x3d, 0x3b, 0x3c, 0x58, 0x64, 0x44, 0x42, 0x40, 0x3e, 0x0f, 0x29, 0x59,
        0x65, 0x38, 0x2a, 0x70, 0x1d, 0x10, 0x02, 0x5a, 0x66, 0x71, 0x2c, 0x1f, 0x1e, 0x11, 0x03, 0x5b,
        0x67, 0x2e, 0x2d, 0x20, 0x12, 0x05, 0x04, 0x5c, 0x68, 0x39, 0x2f, 0x21, 0x14, 0x13, 0x06, 0x5d,
        0x69, 0x31, 0x30, 0x23, 0x22, 0x15, 0x07, 0x5e, 0x6a, 0x72, 0x32, 0x24, 0x16, 0x08, 0x09, 0x5f,
        0x6b, 0x33, 0x25, 0x17, 0x18, 0x0b, 0x0a, 0x60, 0x6c, 0x34, 0x35, 0x26, 0x27, 0x19, 0x0c, 0x61,
        0x6d, 0x73, 0x28, 0x74, 0x1a, 0x0d, 0x62, 0x6e, 0x3a, 0x36, 0x1c, 0x1b, 0x75, 0x2b, 0x63, 0x76,
        0x55, 0x56, 0x77, 0x78, 0x79, 0x7a, 0x0e, 0x7b, 0x7c, 0x4f, 0x7d, 0x4b, 0x47, 0x7e, 0x7f, 0x6f,
        0x52, 0x53, 0x50, 0x4c, 0x4d, 0x48, 0x01, 0x45, 0x57, 0x4e, 0x51, 0x4a, 0x37, 0x49, 0x46, 0x54,
        0x80, 0x81, 0x82, 0x41, 0x54, 0x85, 0x86, 0x87,
    ];
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = if i < HEAD.len() { HEAD[i] } else { i as u8 };
        i += 1;
    }
    t
}

/// A PS/2 device's output queue (QEMU `PS2Queue`). The first `replies`
/// bytes are keyboard command replies, which were queued ahead of any
/// pending scan codes.
#[derive(Default)]
struct Queue {
    data: VecDeque<u8>,
    replies: usize,
    /// Returned when the queue is read while empty (QEMU returns the byte
    /// before the read pointer, i.e. normally the last byte read).
    last: u8,
}

impl Queue {
    fn free(&self) -> usize {
        QUEUE_SIZE.saturating_sub(self.data.len())
    }

    /// Queue `bytes` only if all of them fit (QEMU `ps2_queue_N`).
    fn push(&mut self, bytes: &[u8]) -> bool {
        if self.free() < bytes.len() {
            return false;
        }
        self.data.extend(bytes);
        true
    }

    /// Put command replies in front of everything else (`ps2_cqueue_N`).
    fn push_replies(&mut self, bytes: &[u8]) {
        for &b in bytes.iter().rev() {
            self.data.push_front(b);
        }
        self.replies = bytes.len();
    }

    /// Drop unread command replies (`ps2_cqueue_reset`).
    fn drop_replies(&mut self) {
        self.data.drain(..self.replies);
        self.replies = 0;
    }

    fn clear(&mut self) {
        self.data.clear();
        self.replies = 0;
    }

    fn pop(&mut self) -> Option<u8> {
        let b = self.data.pop_front()?;
        self.replies = self.replies.saturating_sub(1);
        self.last = b;
        Some(b)
    }
}

/// The PS/2 keyboard (QEMU `PS2KbdState`).
struct Keyboard {
    queue: Queue,
    /// Command waiting for its parameter byte.
    write_cmd: Option<u8>,
    scan_enabled: bool,
    scancode_set: u8,
    /// Controller translation (command byte bit 6), pushed in by the i8042.
    translate: bool,
    /// Translation saw 0xF0 and will set bit 7 on the next byte.
    need_high_bit: bool,
    ledstate: u8,
}

impl Keyboard {
    fn new() -> Self {
        Self {
            queue: Queue::default(),
            write_cmd: None,
            scan_enabled: true,
            scancode_set: 2,
            translate: false,
            need_high_bit: false,
            ledstate: 0,
        }
    }

    fn reset(&mut self) {
        self.scan_enabled = true;
        self.scancode_set = 2;
        self.queue.clear();
        self.ledstate = 0;
    }

    /// QEMU `ps2_write_keyboard`. Returns true if the IRQ was raised.
    fn write(&mut self, val: u8) -> bool {
        self.queue.drop_replies();
        let reply: &[u8] = match self.write_cmd.take() {
            None => match val {
                0x00 => &[REPLY_ACK],
                0x05 => &[REPLY_RESEND],
                0xf2 => &[REPLY_ACK, REPLY_ID, if self.translate { 0x41 } else { 0x83 }],
                0xee => &[0xee],
                0xf4 => {
                    self.scan_enabled = true;
                    &[REPLY_ACK]
                }
                0xf0 | 0xed | 0xf3 | 0xfc => {
                    self.write_cmd = Some(val);
                    &[REPLY_ACK]
                }
                0xf5 => {
                    self.reset();
                    self.scan_enabled = false;
                    &[REPLY_ACK]
                }
                0xf6 => {
                    self.reset();
                    &[REPLY_ACK]
                }
                0xff => {
                    self.reset();
                    &[REPLY_ACK, REPLY_POR]
                }
                0xfa => &[REPLY_ACK],
                _ => &[REPLY_RESEND],
            },
            Some(0xf0) => match val {
                0 => {
                    let set = self.scancode_set;
                    let set = if self.translate { TRANSLATE[set as usize] } else { set };
                    self.queue.push_replies(&[REPLY_ACK, set]);
                    return true;
                }
                1..=3 => {
                    self.scancode_set = val;
                    &[REPLY_ACK]
                }
                _ => &[REPLY_RESEND],
            },
            Some(0xed) => {
                self.ledstate = val;
                &[REPLY_ACK]
            }
            // 0xF3 typematic rate, 0xFC set make/break: accepted and ignored.
            Some(_) => &[REPLY_ACK],
        };
        self.queue.push_replies(reply);
        true
    }

    /// One byte from the keyboard towards the controller, through the
    /// controller's translation (QEMU `ps2_put_keycode`).
    fn put_keycode(&mut self, code: u8) {
        let b = if self.translate {
            if code == 0xf0 {
                self.need_high_bit = true;
                return;
            }
            let t = TRANSLATE[code as usize];
            if std::mem::take(&mut self.need_high_bit) {
                t | 0x80
            } else {
                t
            }
        } else {
            code
        };
        self.queue.push(&[b]);
    }
}

/// Set 2 bytes of one key event converted to set 1 (the same mapping the
/// controller's translation applies).
pub(crate) fn set2_to_set1(set2: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(set2.len());
    let mut release = false;
    for &b in set2 {
        match b {
            0xf0 => release = true,
            0xe0 | 0xe1 => out.push(b),
            _ => out.push(TRANSLATE[b as usize] | if std::mem::take(&mut release) { 0x80 } else { 0 }),
        }
    }
    out
}

// Mouse status byte (reported by 0xE9).
const MOUSE_STATUS_REMOTE: u8 = 0x40;
const MOUSE_STATUS_ENABLED: u8 = 0x20;
const MOUSE_STATUS_SCALE21: u8 = 0x10;

/// The PS/2 mouse (QEMU `PS2MouseState`).
struct Mouse {
    queue: Queue,
    write_cmd: Option<u8>,
    status: u8,
    resolution: u8,
    sample_rate: u8,
    wrap: bool,
    /// Device ID: 0 plain PS/2, 3 IntelliMouse (wheel), 4 IntelliMouse Explorer.
    mouse_type: u8,
    /// Progress through the sample-rate "knock" that selects the ID.
    detect_state: u8,
    dx: i32,
    dy: i32,
    dz: i32,
    buttons: u8,
}

impl Mouse {
    fn new() -> Self {
        Self {
            queue: Queue::default(),
            write_cmd: None,
            status: 0,
            resolution: 0,
            sample_rate: 0,
            wrap: false,
            mouse_type: 0,
            detect_state: 0,
            dx: 0,
            dy: 0,
            dz: 0,
            buttons: 0,
        }
    }

    /// QEMU `ps2_write_mouse`. Returns true if the IRQ was raised.
    fn write(&mut self, val: u8) -> bool {
        match self.write_cmd.take() {
            Some(0xf3) => {
                self.sample_rate = val;
                self.detect_state = match (self.detect_state, val) {
                    (0, 200) => 1,
                    (1, 100) => 2,
                    (1, 200) => 3,
                    (2, 80) => {
                        self.mouse_type = 3;
                        0
                    }
                    (3, 80) => {
                        self.mouse_type = 4;
                        0
                    }
                    _ => 0,
                };
                return self.queue.push(&[REPLY_ACK]);
            }
            Some(_) => {
                // 0xE8 set resolution.
                self.resolution = val;
                return self.queue.push(&[REPLY_ACK]);
            }
            None => {}
        }
        if self.wrap {
            if val == 0xec {
                self.wrap = false;
                return self.queue.push(&[REPLY_ACK]);
            } else if val != 0xff {
                return self.queue.push(&[val]);
            }
        }
        match val {
            0xe6 => self.status &= !MOUSE_STATUS_SCALE21,
            0xe7 => self.status |= MOUSE_STATUS_SCALE21,
            0xea => self.status &= !MOUSE_STATUS_REMOTE,
            0xee => self.wrap = true,
            0xf0 => self.status |= MOUSE_STATUS_REMOTE,
            0xf2 => return self.queue.push(&[REPLY_ACK, self.mouse_type]),
            0xe8 | 0xf3 => self.write_cmd = Some(val),
            0xe9 => {
                return self.queue.push(&[REPLY_ACK, self.status, self.resolution, self.sample_rate])
            }
            0xeb => {
                let raised = self.queue.push(&[REPLY_ACK]);
                return self.send_packet() || raised;
            }
            0xf4 => self.status |= MOUSE_STATUS_ENABLED,
            0xf5 => self.status &= !MOUSE_STATUS_ENABLED,
            0xf6 => {
                self.sample_rate = 100;
                self.resolution = 2;
                self.status = 0;
            }
            0xff => {
                self.sample_rate = 100;
                self.resolution = 2;
                self.status = 0;
                self.mouse_type = 0;
                self.queue.clear();
                return self.queue.push(&[REPLY_ACK, REPLY_POR, self.mouse_type]);
            }
            // QEMU ignores unknown mouse commands without a reply.
            _ => return false,
        }
        self.queue.push(&[REPLY_ACK])
    }

    /// Queue one movement packet from the accumulated deltas (QEMU
    /// `ps2_mouse_send_packet`). Returns false if the queue has no room.
    fn send_packet(&mut self) -> bool {
        let needed = if self.mouse_type != 0 { 4 } else { 3 };
        if self.queue.free() < needed {
            return false;
        }
        let dx = self.dx.clamp(-127, 127);
        let dy = self.dy.clamp(-127, 127);
        let b0 = 0x08 | (u8::from(dx < 0) << 4) | (u8::from(dy < 0) << 5) | (self.buttons & 0x07);
        let mut pkt = vec![b0, dx as u8, dy as u8];
        match self.mouse_type {
            3 => {
                let dz = self.dz.clamp(-127, 127);
                pkt.push(dz as u8);
                self.dz -= dz;
            }
            4 => {
                let dz = self.dz.clamp(-7, 7);
                pkt.push((dz as u8 & 0x0f) | ((self.buttons & 0x18) << 1));
                self.dz -= dz;
            }
            _ => self.dz = 0,
        }
        self.queue.push(&pkt);
        self.dx -= dx;
        self.dy -= dy;
        true
    }

    /// QEMU `ps2_mouse_event` + `ps2_mouse_sync`. Returns true if the IRQ
    /// was raised.
    fn input(&mut self, dx: i32, dy: i32, dz: i32, buttons: u8) -> bool {
        if self.status & MOUSE_STATUS_ENABLED == 0 {
            return false;
        }
        self.dx = self.dx.saturating_add(dx);
        self.dy = self.dy.saturating_add(dy);
        self.dz = self.dz.saturating_add(dz);
        self.buttons = buttons & 0x1f;
        if self.status & MOUSE_STATUS_REMOTE != 0 {
            return false;
        }
        let mut raised = false;
        while self.send_packet() {
            raised = true;
            if self.dx == 0 && self.dy == 0 && self.dz == 0 {
                break;
            }
        }
        raised
    }
}

/// The i8042 controller with its keyboard and mouse.
pub struct Ps2 {
    /// Command byte (read with 0x20, written with 0x60).
    mode: u8,
    status: u8,
    outport: u8,
    pending: u8,
    obsrc: ObSource,
    /// Last byte delivered through port 0x60 (re-read when the buffer is empty).
    obdata: u8,
    /// Controller-generated byte (QEMU `cbdata`).
    cbdata: u8,
    /// Controller command waiting for its data byte on port 0x60.
    write_cmd: Option<u8>,
    irq_kbd: bool,
    irq_mouse: bool,
    edge_kbd: bool,
    edge_mouse: bool,
    reset_request: bool,
    kbd: Keyboard,
    mouse: Mouse,
}

impl Default for Ps2 {
    fn default() -> Self {
        Self::new()
    }
}

impl Ps2 {
    /// Controller, keyboard and mouse in their power-on state: command byte
    /// 0x03 (both IRQs enabled, no translation), status 0x18, output port
    /// 0x03 (not in reset, A20 on).
    pub fn new() -> Self {
        Self {
            mode: MODE_KBD_INT | MODE_MOUSE_INT,
            status: STAT_CMD | STAT_UNLOCKED,
            outport: OUT_RESET | OUT_A20,
            pending: 0,
            obsrc: ObSource::None,
            obdata: 0,
            cbdata: 0,
            write_cmd: None,
            irq_kbd: false,
            irq_mouse: false,
            edge_kbd: false,
            edge_mouse: false,
            reset_request: false,
            kbd: Keyboard::new(),
            mouse: Mouse::new(),
        }
    }

    /// Port read: 0x60 data, 0x64 status.
    pub fn read(&mut self, port: u16) -> u8 {
        match port {
            0x60 => self.read_data(),
            // QEMU never clears STAT_CMD, so bit 3 always reads 1.
            0x64 => self.status,
            _ => {
                unhandled("ps2 read", port.into(), 1, None);
                0xff
            }
        }
    }

    /// Port write: 0x60 data (to the keyboard, or the parameter of a
    /// controller command), 0x64 controller command.
    pub fn write(&mut self, port: u16, val: u8) {
        match port {
            0x60 => self.write_data(val),
            0x64 => self.write_command(val),
            _ => unhandled("ps2 write", port.into(), 1, Some(val.into())),
        }
    }

    /// Level of the keyboard interrupt line (IRQ1).
    pub fn irq1(&self) -> bool {
        self.irq_kbd
    }

    /// Level of the mouse interrupt line (IRQ12).
    pub fn irq12(&self) -> bool {
        self.irq_mouse
    }

    /// True once if IRQ1 rose since the last call. Reading port 0x60 with
    /// more data waiting drops the line and raises it again within the same
    /// access, which an edge-triggered PIC sees as a new interrupt but
    /// [`Ps2::irq1`] alone can't show; the board should pulse the PIC input
    /// when this returns true.
    pub fn take_irq1_edge(&mut self) -> bool {
        std::mem::take(&mut self.edge_kbd)
    }

    /// Like [`Ps2::take_irq1_edge`], for IRQ12.
    pub fn take_irq12_edge(&mut self) -> bool {
        std::mem::take(&mut self.edge_mouse)
    }

    /// True once after the guest asked for a CPU reset (command 0xFE or any
    /// 0xF0-0xFF pulse with bit 0 clear, or 0xD1 with output port bit 0 clear).
    pub fn take_reset_request(&mut self) -> bool {
        std::mem::take(&mut self.reset_request)
    }

    /// State of the A20 gate (output port bit 1).
    pub fn a20(&self) -> bool {
        self.outport & OUT_A20 != 0
    }

    /// Inject one key event as scan code set 2 bytes (e.g. `[0x1C]` press A,
    /// `[0xF0, 0x1C]` release). The keyboard converts it to its current scan
    /// code set and the controller translates it if command byte bit 6 is set.
    /// Ignored while scanning is disabled (0xF5), as in QEMU.
    ///
    /// Simplification: in scan code set 3 the set 2 bytes are sent unchanged.
    pub fn keyboard_input(&mut self, set2: &[u8]) {
        if !self.kbd.scan_enabled {
            return;
        }
        let codes = match self.kbd.scancode_set {
            1 => set2_to_set1(set2),
            _ => set2.to_vec(),
        };
        let before = self.kbd.queue.data.len();
        for c in codes {
            self.kbd.put_keycode(c);
        }
        if self.kbd.queue.data.len() != before {
            self.kbd_irq(true);
        }
    }

    /// Inject relative mouse motion: `dx` right, `dy` up (PS/2 convention),
    /// `dz` wheel (positive = down/towards the user, PS/2 convention),
    /// `buttons` bit 0 left, 1 right, 2 middle, 3/4 side buttons. Ignored
    /// unless the guest enabled reporting (0xF4); in stream mode packets
    /// are queued at once, several if a delta exceeds ±127. Packet length
    /// and the fourth byte follow the mouse ID (0, 3 or 4).
    pub fn mouse_input(&mut self, dx: i32, dy: i32, dz: i32, buttons: u8) {
        if self.mouse.input(dx, dy, dz, buttons) {
            self.aux_irq(true);
        }
    }

    // ---- controller internals (QEMU pckbd.c) ----

    fn read_data(&mut self) -> u8 {
        if self.status & STAT_OBF != 0 {
            self.deassert_irq();
            match self.obsrc {
                ObSource::Kbd => {
                    self.obdata = self.dev_read(false);
                }
                ObSource::Mouse => {
                    self.obdata = self.dev_read(true);
                }
                ObSource::Ctrl => {
                    self.obdata = self.dequeue_ctrl();
                }
                ObSource::None => {}
            }
        }
        self.obdata
    }

    /// QEMU `ps2_read_data`: pop a byte and re-signal if more are waiting.
    fn dev_read(&mut self, aux: bool) -> u8 {
        let q = if aux { &mut self.mouse.queue } else { &mut self.kbd.queue };
        match q.pop() {
            Some(b) => {
                let more = !q.data.is_empty();
                self.dev_irq(aux, false);
                if more {
                    self.dev_irq(aux, true);
                }
                b
            }
            None => q.last,
        }
    }

    fn dev_irq(&mut self, aux: bool, level: bool) {
        if aux {
            self.aux_irq(level)
        } else {
            self.kbd_irq(level)
        }
    }

    fn write_data(&mut self, val: u8) {
        match self.write_cmd.take() {
            None => {
                if self.kbd.write(val) {
                    self.kbd_irq(true);
                }
                // Sending data to the keyboard re-enables its interface.
                self.mode &= !MODE_DISABLE_KBD;
                self.safe_update_irq();
            }
            Some(CCMD_WRITE_MODE) => {
                self.mode = val;
                self.kbd.translate = val & MODE_KCC != 0;
                self.update_irq_lines();
                self.safe_update_irq();
            }
            Some(CCMD_WRITE_OBUF) => self.queue_ctrl(val, false),
            Some(CCMD_WRITE_AUX_OBUF) => self.queue_ctrl(val, true),
            Some(CCMD_WRITE_OUTPORT) => {
                self.outport = val;
                if val & OUT_RESET == 0 {
                    self.reset_request = true;
                }
            }
            Some(CCMD_WRITE_MOUSE) => {
                if self.mouse.write(val) {
                    self.aux_irq(true);
                }
                self.mode &= !MODE_DISABLE_MOUSE;
                self.safe_update_irq();
            }
            Some(_) => {}
        }
    }

    fn write_command(&mut self, mut val: u8) {
        // 0xF0-0xFF pulse output port bits 3-0 low; only bit 0 (reset) matters.
        if val & 0xf0 == 0xf0 {
            val = if val & 1 == 0 { 0xfe } else { 0xff };
        }
        match val {
            0x20 => self.queue_ctrl(self.mode, false),
            CCMD_WRITE_MODE | CCMD_WRITE_OBUF | CCMD_WRITE_AUX_OBUF | CCMD_WRITE_MOUSE
            | CCMD_WRITE_OUTPORT => self.write_cmd = Some(val),
            0xa7 => self.mode |= MODE_DISABLE_MOUSE,
            0xa8 => {
                self.mode &= !MODE_DISABLE_MOUSE;
                self.safe_update_irq();
            }
            0xa9 => self.queue_ctrl(0x00, false),
            0xaa => {
                self.status |= STAT_SELFTEST;
                self.queue_ctrl(0x55, false);
            }
            0xab => self.queue_ctrl(0x00, false),
            0xad => self.mode |= MODE_DISABLE_KBD,
            0xae => {
                self.mode &= !MODE_DISABLE_KBD;
                self.safe_update_irq();
            }
            0xc0 => self.queue_ctrl(0x80, false),
            0xd0 => self.queue_ctrl(self.outport, false),
            0xdd => self.outport &= !OUT_A20,
            0xdf => self.outport |= OUT_A20,
            0xfe => self.reset_request = true,
            0xff => {}
            _ => unhandled("i8042 cmd", val.into(), 1, None),
        }
    }

    /// QEMU `kbd_queue` with extended state: a controller-generated byte.
    fn queue_ctrl(&mut self, b: u8, aux: bool) {
        self.cbdata = b;
        self.pending &= !(PENDING_CTRL_KBD | PENDING_CTRL_AUX);
        self.pending |= if aux { PENDING_CTRL_AUX } else { PENDING_CTRL_KBD };
        self.safe_update_irq();
    }

    fn dequeue_ctrl(&mut self) -> u8 {
        self.pending &= !(PENDING_CTRL_KBD | PENDING_CTRL_AUX);
        if self.pending_visible() != 0 {
            self.update_irq();
        }
        self.cbdata
    }

    /// Pending sources not masked by a disabled interface (`kbd_pending`).
    fn pending_visible(&self) -> u8 {
        self.pending & !(self.mode & (PENDING_KBD | PENDING_AUX))
    }

    fn kbd_irq(&mut self, level: bool) {
        if level {
            self.pending |= PENDING_KBD;
        } else {
            self.pending &= !PENDING_KBD;
        }
        self.safe_update_irq();
    }

    fn aux_irq(&mut self, level: bool) {
        if level {
            self.pending |= PENDING_AUX;
        } else {
            self.pending &= !PENDING_AUX;
        }
        self.safe_update_irq();
    }

    /// Load the output buffer if it is empty and something is pending.
    fn safe_update_irq(&mut self) {
        if self.status & STAT_OBF != 0 {
            return;
        }
        if self.pending_visible() != 0 {
            self.update_irq();
        }
    }

    /// Choose the next output-buffer source and set OBF / AUX_OBF.
    fn update_irq(&mut self) {
        let pending = self.pending_visible();
        self.status &= !(STAT_OBF | STAT_MOUSE_OBF);
        self.outport &= !(OUT_OBF | OUT_MOUSE_OBF);
        if pending != 0 {
            self.status |= STAT_OBF;
            self.outport |= OUT_OBF;
            let aux = if pending & PENDING_CTRL_KBD != 0 {
                self.obsrc = ObSource::Ctrl;
                false
            } else if pending & PENDING_CTRL_AUX != 0 {
                self.obsrc = ObSource::Ctrl;
                true
            } else if pending & PENDING_KBD != 0 {
                self.obsrc = ObSource::Kbd;
                false
            } else {
                self.obsrc = ObSource::Mouse;
                true
            };
            if aux {
                self.status |= STAT_MOUSE_OBF;
                self.outport |= OUT_MOUSE_OBF;
            }
        }
        self.update_irq_lines();
    }

    fn deassert_irq(&mut self) {
        self.status &= !(STAT_OBF | STAT_MOUSE_OBF);
        self.outport &= !(OUT_OBF | OUT_MOUSE_OBF);
        self.update_irq_lines();
    }

    /// Drive IRQ1/IRQ12 from the output buffer state. Like QEMU, this runs
    /// only at the points above, so e.g. 0xAD leaves a raised IRQ1 raised.
    fn update_irq_lines(&mut self) {
        let mut kbd = false;
        let mut mouse = false;
        if self.status & STAT_OBF != 0 {
            if self.status & STAT_MOUSE_OBF != 0 {
                mouse = self.mode & MODE_MOUSE_INT != 0;
            } else {
                kbd = self.mode & MODE_KBD_INT != 0 && self.mode & MODE_DISABLE_KBD == 0;
            }
        }
        self.edge_kbd |= kbd && !self.irq_kbd;
        self.edge_mouse |= mouse && !self.irq_mouse;
        self.irq_kbd = kbd;
        self.irq_mouse = mouse;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(p: &mut Ps2, c: u8) {
        assert_eq!(p.read(0x64) & 0x02, 0, "IBF is never set");
        p.write(0x64, c);
    }

    fn data(p: &mut Ps2, d: u8) {
        p.write(0x60, d);
    }

    /// Read one byte, checking OBF first as a polling guest does.
    fn recv(p: &mut Ps2) -> u8 {
        assert_ne!(p.read(0x64) & STAT_OBF, 0, "expected data");
        p.read(0x60)
    }

    fn drain(p: &mut Ps2) -> Vec<u8> {
        let mut v = Vec::new();
        while p.read(0x64) & STAT_OBF != 0 {
            v.push(p.read(0x60));
        }
        v
    }

    /// Send a byte to the mouse and collect its reply bytes.
    fn mouse_cmd(p: &mut Ps2, b: u8) -> Vec<u8> {
        cmd(p, 0xd4);
        data(p, b);
        let mut v = Vec::new();
        while p.read(0x64) & STAT_OBF != 0 {
            assert_ne!(p.read(0x64) & STAT_MOUSE_OBF, 0, "mouse byte without AUX_OBF");
            v.push(p.read(0x60));
        }
        v
    }

    #[test]
    fn reset_state_and_self_test() {
        let mut p = Ps2::new();
        assert_eq!(p.read(0x64), 0x18);
        assert!(p.a20());
        cmd(&mut p, 0xad);
        cmd(&mut p, 0xa7);
        cmd(&mut p, 0xaa);
        assert_eq!(p.read(0x64), 0x1d);
        assert_eq!(p.read(0x60), 0x55);
        assert_eq!(p.read(0x64), 0x1c);
        cmd(&mut p, 0xab);
        assert_eq!(recv(&mut p), 0x00);
        cmd(&mut p, 0xa9);
        assert_eq!(recv(&mut p), 0x00);
        cmd(&mut p, 0xc0);
        assert_eq!(recv(&mut p), 0x80);
        // Empty buffer re-reads the last byte.
        assert_eq!(p.read(0x60), 0x80);
    }

    #[test]
    fn command_byte() {
        let mut p = Ps2::new();
        cmd(&mut p, 0x20);
        assert_eq!(recv(&mut p), 0x03);
        cmd(&mut p, 0x60);
        data(&mut p, 0x61);
        cmd(&mut p, 0x20);
        assert!(p.irq1(), "controller data raises IRQ1 when enabled");
        assert_eq!(recv(&mut p), 0x61);
        assert!(!p.irq1());
        cmd(&mut p, 0x60);
        data(&mut p, 0x30);
        cmd(&mut p, 0x20);
        assert!(!p.irq1());
        assert_eq!(recv(&mut p), 0x30);
    }

    #[test]
    fn output_port_and_reset() {
        let mut p = Ps2::new();
        cmd(&mut p, 0xd0);
        assert_eq!(recv(&mut p), 0x03, "OBF bit is set only after the byte is latched");
        cmd(&mut p, 0xdd);
        assert!(!p.a20());
        cmd(&mut p, 0xd1);
        data(&mut p, 0x03);
        assert!(p.a20());
        assert!(!p.take_reset_request());
        cmd(&mut p, 0xff);
        cmd(&mut p, 0xf1);
        assert!(!p.take_reset_request());
        cmd(&mut p, 0xfe);
        assert!(p.take_reset_request());
        assert!(!p.take_reset_request());
        cmd(&mut p, 0xf0);
        assert!(p.take_reset_request());
        cmd(&mut p, 0xd1);
        data(&mut p, 0x02);
        assert!(p.take_reset_request());
    }

    #[test]
    fn write_obuf_commands() {
        let mut p = Ps2::new();
        cmd(&mut p, 0xd2);
        data(&mut p, 0x12);
        assert_eq!(p.read(0x64) & STAT_MOUSE_OBF, 0);
        assert!(p.irq1());
        assert_eq!(p.read(0x60), 0x12);
        cmd(&mut p, 0xd3);
        data(&mut p, 0x34);
        assert_ne!(p.read(0x64) & STAT_MOUSE_OBF, 0);
        assert!(p.irq12());
        assert_eq!(p.read(0x60), 0x34);
        assert!(!p.irq12());
    }

    #[test]
    fn keyboard_reset_identify_translation() {
        let mut p = Ps2::new();
        data(&mut p, 0xff);
        assert_eq!(drain(&mut p), [0xfa, 0xaa]);
        // Translation off (reset command byte).
        data(&mut p, 0xf2);
        assert_eq!(drain(&mut p), [0xfa, 0xab, 0x83]);
        data(&mut p, 0xf0);
        data(&mut p, 0x00);
        assert_eq!(drain(&mut p), [0xfa, 0x02]);
        // Translation on.
        cmd(&mut p, 0x60);
        data(&mut p, 0x61);
        data(&mut p, 0xf2);
        assert_eq!(drain(&mut p), [0xfa, 0xab, 0x41]);
        data(&mut p, 0xf0);
        assert_eq!(drain(&mut p), [0xfa]);
        data(&mut p, 0x02);
        assert_eq!(drain(&mut p), [0xfa]);
        data(&mut p, 0xf0);
        data(&mut p, 0x00);
        assert_eq!(drain(&mut p), [0xfa, 0x41]);
        data(&mut p, 0xf3);
        data(&mut p, 0x00);
        data(&mut p, 0xed);
        data(&mut p, 0x07);
        assert_eq!(drain(&mut p), [0xfa], "unread reply replaced by the next");
        data(&mut p, 0xee);
        assert_eq!(drain(&mut p), [0xee]);
        data(&mut p, 0xf5);
        data(&mut p, 0xf4);
        data(&mut p, 0xf6);
        assert_eq!(drain(&mut p), [0xfa]);
        data(&mut p, 0xab);
        assert_eq!(drain(&mut p), [0xfe]);
    }

    #[test]
    fn translated_key_make_break() {
        let mut p = Ps2::new();
        cmd(&mut p, 0x60);
        data(&mut p, 0x61); // KBD_INT | KCC | mouse disabled
        data(&mut p, 0xf0);
        data(&mut p, 0x02);
        drain(&mut p);
        assert!(!p.irq1());

        p.keyboard_input(&[0x1c]); // A
        assert!(p.irq1());
        assert!(p.take_irq1_edge());
        assert_eq!(p.read(0x64) & STAT_MOUSE_OBF, 0);
        assert_eq!(p.read(0x60), 0x1e);
        assert!(!p.irq1());
        p.keyboard_input(&[0xf0, 0x1c]);
        assert_eq!(drain(&mut p), [0x9e]);
        p.keyboard_input(&[0xe0, 0x75]); // Up
        p.keyboard_input(&[0xe0, 0xf0, 0x75]);
        // Two bytes pending: reading the first re-raises IRQ1 at once.
        p.take_irq1_edge();
        assert_eq!(p.read(0x60), 0xe0);
        assert!(p.irq1());
        assert!(p.take_irq1_edge(), "the PIC must see a new edge");
        assert_eq!(drain(&mut p), [0x48, 0xe0, 0xc8]);

        // Without translation the set 2 bytes arrive unchanged.
        cmd(&mut p, 0x60);
        data(&mut p, 0x21);
        p.keyboard_input(&[0xf0, 0x1c]);
        assert_eq!(drain(&mut p), [0xf0, 0x1c]);
        // Scan set 1 without translation.
        data(&mut p, 0xf0);
        data(&mut p, 0x01);
        drain(&mut p);
        p.keyboard_input(&[0xf0, 0x1c]);
        assert_eq!(drain(&mut p), [0x9e]);
        // Disabled scanning drops keys.
        data(&mut p, 0xf5);
        drain(&mut p);
        p.keyboard_input(&[0x1c]);
        assert!(drain(&mut p).is_empty());
    }

    #[test]
    fn disabled_keyboard_holds_data() {
        let mut p = Ps2::new();
        cmd(&mut p, 0xad);
        p.keyboard_input(&[0x1c]);
        assert_eq!(p.read(0x64) & STAT_OBF, 0);
        cmd(&mut p, 0xae);
        assert_eq!(recv(&mut p), 0x1c);
    }

    #[test]
    fn templeos_mouse_probe() {
        let mut p = Ps2::new();
        // Keyboard.HC style setup: disable mouse port, enable IRQ12.
        cmd(&mut p, 0xa7);
        cmd(&mut p, 0xae);
        cmd(&mut p, 0xa8);
        cmd(&mut p, 0x20);
        let cb = recv(&mut p);
        cmd(&mut p, 0x60);
        data(&mut p, (cb | 0x02) & !0x20);
        cmd(&mut p, 0xad);

        assert_eq!(mouse_cmd(&mut p, 0xff), [0xfa, 0xaa, 0x00]);
        for rate in [200, 100, 80] {
            assert_eq!(mouse_cmd(&mut p, 0xf3), [0xfa]);
            assert_eq!(mouse_cmd(&mut p, rate), [0xfa]);
        }
        assert_eq!(mouse_cmd(&mut p, 0xf2), [0xfa, 0x03]);
        assert_eq!(mouse_cmd(&mut p, 0xf3), [0xfa]);
        assert_eq!(mouse_cmd(&mut p, 10), [0xfa]);
        assert_eq!(mouse_cmd(&mut p, 0xf2), [0xfa, 0x03]);
        assert_eq!(mouse_cmd(&mut p, 0xe8), [0xfa]);
        assert_eq!(mouse_cmd(&mut p, 0x03), [0xfa]);
        assert_eq!(mouse_cmd(&mut p, 0xe6), [0xfa]);
        assert_eq!(mouse_cmd(&mut p, 0xf3), [0xfa]);
        assert_eq!(mouse_cmd(&mut p, 100), [0xfa]);
        assert_eq!(mouse_cmd(&mut p, 0xf4), [0xfa]);
        assert_eq!(mouse_cmd(&mut p, 0xe9), [0xfa, 0x20, 0x03, 100]);
        cmd(&mut p, 0xae);

        // A 4-byte packet, AUX_OBF set on each byte, IRQ12 per byte.
        p.mouse_input(5, -3, 1, 0x01);
        assert!(p.irq12());
        assert!(!p.irq1());
        let mut pkt = Vec::new();
        for _ in 0..4 {
            assert_eq!(p.read(0x64) & (STAT_OBF | STAT_MOUSE_OBF), 0x21);
            assert!(p.irq12());
            pkt.push(p.read(0x60));
        }
        assert_eq!(pkt, [0x29, 5, 0xfd, 1]);
        assert!(!p.irq12());
        assert_eq!(p.read(0x64) & STAT_OBF, 0);
    }

    #[test]
    fn mouse_explorer_id_and_packets() {
        let mut p = Ps2::new();
        mouse_cmd(&mut p, 0xff);
        for rate in [200, 200, 80] {
            mouse_cmd(&mut p, 0xf3);
            mouse_cmd(&mut p, rate);
        }
        assert_eq!(mouse_cmd(&mut p, 0xf2), [0xfa, 0x04]);
        mouse_cmd(&mut p, 0xf4);
        p.mouse_input(0, 0, -2, 0x18);
        assert_eq!(drain(&mut p), [0x08, 0, 0, 0x3e]);
        // Large motion is split into several packets.
        p.mouse_input(200, 0, 0, 0);
        assert_eq!(drain(&mut p), [0x08, 127, 0, 0, 0x08, 73, 0, 0]);
        // Reset returns to ID 0 and 3-byte packets.
        assert_eq!(mouse_cmd(&mut p, 0xff), [0xfa, 0xaa, 0x00]);
        p.mouse_input(1, 1, 0, 0);
        assert!(drain(&mut p).is_empty(), "reporting disabled after reset");
        mouse_cmd(&mut p, 0xf4);
        p.mouse_input(-1, 1, 0, 0x02);
        assert_eq!(drain(&mut p), [0x1a, 0xff, 1]);
    }

    #[test]
    fn keyboard_data_before_mouse_data() {
        let mut p = Ps2::new();
        mouse_cmd(&mut p, 0xf4);
        p.mouse_input(1, 0, 0, 0);
        p.keyboard_input(&[0x1c]);
        // Mouse byte already latched in the output buffer goes first.
        assert_ne!(p.read(0x64) & STAT_MOUSE_OBF, 0);
        assert_eq!(p.read(0x60), 0x08);
        // Then the keyboard takes priority over the rest of the packet.
        assert_eq!(p.read(0x64) & STAT_MOUSE_OBF, 0);
        assert_eq!(p.read(0x60), 0x1c);
        assert_eq!(drain(&mut p), [1, 0]);
    }

    #[test]
    fn mouse_wrap_and_remote() {
        let mut p = Ps2::new();
        assert_eq!(mouse_cmd(&mut p, 0xee), [0xfa]);
        assert_eq!(mouse_cmd(&mut p, 0x42), [0x42]);
        assert_eq!(mouse_cmd(&mut p, 0xec), [0xfa]);
        assert_eq!(mouse_cmd(&mut p, 0xf0), [0xfa]);
        assert_eq!(mouse_cmd(&mut p, 0xf4), [0xfa]);
        p.mouse_input(3, 0, 0, 0);
        assert!(drain(&mut p).is_empty());
        assert_eq!(mouse_cmd(&mut p, 0xeb), [0xfa, 0x08, 3, 0]);
        assert!(mouse_cmd(&mut p, 0x12).is_empty(), "unknown commands get no reply");
    }
    /// SeaBIOS POST and TempleOS keyboard init as recorded on the reference
    /// machine (ref/boot/trace.log): (is_read, port, value).
    #[test]
    fn reference_trace_i8042() {
        #[rustfmt::skip]
        const TRACE: &[(bool, u16, u8)] = &[
            (true, 0x64, 0x18), (true, 0x64, 0x18), (false, 0x64, 0xad), (true, 0x64, 0x18),
            (false, 0x64, 0xa7), (true, 0x64, 0x18), (true, 0x64, 0x18), (false, 0x64, 0xaa),
            (true, 0x64, 0x1d), (true, 0x60, 0x55), (true, 0x64, 0x1c), (false, 0x64, 0xab),
            (true, 0x64, 0x1d), (true, 0x60, 0x0), (true, 0x64, 0x1c), (false, 0x64, 0x60),
            (true, 0x64, 0x1c), (false, 0x60, 0x30), (true, 0x64, 0x1c), (false, 0x64, 0x60),
            (true, 0x64, 0x1c), (false, 0x60, 0x20), (true, 0x64, 0x1c), (false, 0x60, 0xff),
            (true, 0x64, 0x1d), (true, 0x60, 0xfa), (true, 0x64, 0x1d), (true, 0x60, 0xaa),
            (true, 0x64, 0x1c), (false, 0x64, 0x60), (true, 0x64, 0x1c), (false, 0x60, 0x30),
            (true, 0x64, 0x1c), (false, 0x64, 0x60), (true, 0x64, 0x1c), (false, 0x60, 0x30),
            (true, 0x64, 0x1c), (false, 0x64, 0x60), (true, 0x64, 0x1c), (false, 0x60, 0x20),
            (true, 0x64, 0x1c), (false, 0x60, 0xf5), (true, 0x64, 0x1d), (true, 0x60, 0xfa),
            (true, 0x64, 0x1c), (false, 0x64, 0x60), (true, 0x64, 0x1c), (false, 0x60, 0x30),
            (true, 0x64, 0x1c), (false, 0x64, 0x60), (true, 0x64, 0x1c), (false, 0x60, 0x30),
            (true, 0x64, 0x1c), (false, 0x64, 0x60), (true, 0x64, 0x1c), (false, 0x60, 0x20),
            (true, 0x64, 0x1c), (false, 0x60, 0xf0), (true, 0x64, 0x1d), (true, 0x60, 0xfa),
            (true, 0x64, 0x1c), (false, 0x60, 0x2), (true, 0x64, 0x1d), (true, 0x60, 0xfa),
            (true, 0x64, 0x1c), (false, 0x64, 0x60), (true, 0x64, 0x1c), (false, 0x60, 0x30),
            (true, 0x64, 0x1c), (false, 0x64, 0x60), (true, 0x64, 0x1c), (false, 0x60, 0x70),
            (true, 0x64, 0x1c), (false, 0x64, 0x60), (true, 0x64, 0x1c), (false, 0x60, 0x60),
            (true, 0x64, 0x1c), (false, 0x60, 0xf4), (true, 0x64, 0x1d), (true, 0x60, 0xfa),
            (true, 0x64, 0x1c), (false, 0x64, 0x60), (true, 0x64, 0x1c), (false, 0x60, 0x61),
        ];
        let mut p = Ps2::new();
        for (i, &(is_read, port, val)) in TRACE.iter().enumerate() {
            if is_read {
                assert_eq!(p.read(port), val, "access {i}: read {port:#x}");
            } else {
                p.write(port, val);
            }
        }
    }
}
