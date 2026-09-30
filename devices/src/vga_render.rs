//! Turns the VGA's state into pixels, the way QEMU 8.2's `vga_update_display`
//! does (`vga_draw_text`, `vga_draw_graphic` and the `vga_draw_line*`
//! helpers in hw/display/vga.c and vga-helpers.h), so a frame here and a
//! QEMU `screendump` of the same state have identical pixels.
//!
//! Covered: blanking, text modes (8/9/16-dot cells, two fonts, cursor),
//! 16-colour planar graphics (TempleOS's mode 12h), 4-colour CGA graphics,
//! 256-colour chain-4 (mode 13h) and the VBE 8/15/16/24/32 bpp modes.
//! Not covered (QEMU 8.2 doesn't do them either): horizontal pel panning,
//! text blink attribute, the DAC pixel mask. Big-endian framebuffers (the
//! QEMU extended register) render as little endian.

use crate::vga::{Vga, VgaRegs, VRAM_SIZE};
use crate::Nanos;

/// A rendered frame: `pixels` holds `width * height` 0x00RRGGBB values,
/// row by row (QEMU's x8r8g8b8 surface).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
    pub mode: FrameMode,
}

/// What produced a frame, for the frontend's scaling decisions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FrameMode {
    /// Display disabled (attribute controller PAS clear or screen off).
    #[default]
    Blank,
    Text,
    /// A VGA graphics mode (shown at 4:3 on a real monitor).
    Graphics,
    /// A Bochs VBE (DISPI) mode: square pixels.
    Vbe,
}

impl Frame {
    /// The frame as a binary PPM (P6), byte for byte what QEMU's `screendump`
    /// writes for the same pixels.
    pub fn to_ppm(&self) -> Vec<u8> {
        let mut out = format!("P6\n{} {}\n255\n", self.width, self.height).into_bytes();
        out.reserve(self.pixels.len() * 3);
        for &p in &self.pixels {
            out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
        out
    }

    /// Where to draw this frame in a `cw` x `ch` area: (x, y, width, height).
    ///
    /// - Windowed: the largest whole-number scale that fits (sharp pixels),
    ///   shrinking to fit below 1x, centered.
    /// - Fullscreen: as large as fits with the shape a real monitor gives it:
    ///   4:3 for VGA text and graphics modes (a 720x400 text screen filled a
    ///   4:3 CRT too), square pixels for VBE modes.
    pub fn fit(&self, cw: i32, ch: i32, fullscreen: bool) -> (i32, i32, i32, i32) {
        let (fw, fh) = (self.width as i32, self.height as i32);
        if fw == 0 || fh == 0 || cw <= 0 || ch <= 0 {
            return (0, 0, 0, 0);
        }
        let fit_aspect = |ax: i32, ay: i32| {
            if cw * ay <= ch * ax {
                (cw, cw * ay / ax)
            } else {
                (ch * ax / ay, ch)
            }
        };
        let (w, h) = if fullscreen {
            match self.mode {
                FrameMode::Vbe => fit_aspect(fw, fh),
                _ => fit_aspect(4, 3),
            }
        } else {
            let k = (cw / fw).min(ch / fh);
            if k >= 1 {
                (fw * k, fh * k)
            } else {
                fit_aspect(fw, fh)
            }
        };
        ((cw - w) / 2, (ch - h) / 2, w, h)
    }
}

/// QEMU's `VGA_TEXT_CURSOR_PERIOD_MS`: the cursor toggles every half period.
const CURSOR_PERIOD_NS: Nanos = 1_000_000 * 1000 * 2 * 16 / 60;

/// Rendering state that persists between frames (cursor blink, and the size
/// of the last picture, which a blank screen keeps).
#[derive(Default)]
pub struct Renderer {
    cursor_blink_time: Nanos,
    cursor_visible: bool,
    last_width: u32,
    last_height: u32,
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Render the display as of guest time `now` into `frame`, reusing its
    /// buffer.
    pub fn render(&mut self, vga: &Vga, now: Nanos, frame: &mut Frame) {
        let r = vga.regs();
        if r.ar_index & 0x20 == 0 || r.effective_sr(0x01) & 0x20 != 0 {
            self.blank(frame);
        } else if r.gr[0x06] & 0x01 == 0 {
            self.text(vga, now, frame);
        } else {
            self.graphic(vga, frame);
        }
    }

    fn blank(&mut self, frame: &mut Frame) {
        let (w, h) = if self.last_width == 0 { (640, 480) } else { (self.last_width, self.last_height) };
        resize(frame, w, h, FrameMode::Blank);
        frame.pixels.fill(0);
    }

    fn text(&mut self, vga: &Vga, now: Nanos, frame: &mut Frame) {
        let r = vga.regs();
        let vram = vga.vram();
        let palette = palette16(r);

        // Font bases in plane 2 (QEMU: offset = sel * 8192 * 4 + 2).
        let v = r.effective_sr(0x03);
        let font_a = usize::from((v >> 4) & 1 | (v << 1) & 6) * 8192;
        let font_b = usize::from((v >> 5) & 1 | (v >> 1) & 6) * 8192;

        let cheight = usize::from(r.cr[0x09] & 0x1f) + 1;
        let sr1 = r.effective_sr(0x01);
        let cw = if sr1 & 0x08 != 0 {
            16
        } else if sr1 & 0x01 == 0 {
            9
        } else {
            8
        };
        let width = usize::from(r.cr[0x01]) + 1;
        let height = if r.cr[0x06] == 100 { 100 } else { (vertical_display_end(r) + 1) / cheight };
        if width * height > 160 * 100 || width * cw == 0 || height * cheight == 0 {
            return;
        }
        let (line_offset, start_addr, line_compare) = basic_params(r);
        let cursor = ((usize::from(r.cr[0x0e]) << 8) | usize::from(r.cr[0x0f])).wrapping_sub(start_addr);

        if now >= self.cursor_blink_time {
            self.cursor_blink_time = now + CURSOR_PERIOD_NS / 2;
            self.cursor_visible = !self.cursor_visible;
        }

        let (fw, fh) = ((width * cw) as u32, (height * cheight) as u32);
        resize(frame, fw, fh, FrameMode::Text);
        self.last_width = fw;
        self.last_height = fh;
        let stride = fw as usize;

        let mut offset = start_addr * 4;
        let mut line = 0usize;
        for cy in 0..height {
            let mut src = offset;
            for cx in 0..width {
                if src + 2 > VRAM_SIZE {
                    break;
                }
                let ch = usize::from(vram.get(src));
                let attr = vram.get(src + 1);
                let font = if attr & 0x08 != 0 { font_b } else { font_a };
                let glyph = |row: usize| vram.plane(2)[(font + 32 * ch + row) % vram.plane(2).len()];
                let bg = palette[usize::from(attr >> 4)];
                let fg = palette[usize::from(attr & 0x0f)];
                let dup9 = (0xb0..=0xdf).contains(&ch) && r.ar[0x10] & 0x04 != 0;
                let x0 = cx * cw;
                let y0 = cy * cheight;
                for row in 0..cheight {
                    draw_glyph_row(&mut frame.pixels[(y0 + row) * stride + x0..], glyph(row), cw, fg, bg, dup9);
                }
                let is_cursor = src == (start_addr + cursor) * 4;
                if is_cursor && r.cr[0x0a] & 0x20 == 0 && self.cursor_visible {
                    let start = usize::from(r.cr[0x0a] & 0x1f);
                    let last = usize::from(r.cr[0x0b] & 0x1f).min(cheight - 1);
                    if last >= start && start < cheight {
                        for row in start..=last {
                            draw_glyph_row(&mut frame.pixels[(y0 + row) * stride + x0..], 0xff, cw, fg, bg, true);
                        }
                    }
                }
                src += 4;
            }
            let line1 = line + cheight;
            offset += line_offset;
            if line < line_compare && line1 >= line_compare {
                offset = 0;
            }
            line = line1;
        }
    }

    fn graphic(&mut self, vga: &Vga, frame: &mut Frame) {
        let r = vga.regs();
        let vram = vga.vram();
        let vbe = r.vbe_enabled();
        let shift_control = (r.gr[0x05] >> 5) & 3;
        let double_scan = usize::from(r.cr[0x09] >> 7);
        let multi_scan = if shift_control != 1 {
            ((usize::from(r.cr[0x09] & 0x1f) + 1) << double_scan) - 1
        } else {
            double_scan
        };
        let (mut width, height) = if vbe {
            (
                usize::from(r.vbe_regs[crate::vga::dispi::INDEX_XRES as usize]),
                usize::from(r.vbe_regs[crate::vga::dispi::INDEX_YRES as usize]),
            )
        } else {
            ((usize::from(r.cr[0x01]) + 1) * 8, vertical_display_end(r) + 1)
        };
        let d2 = r.effective_sr(0x01) & 0x08 != 0;
        if shift_control <= 1 && d2 {
            width <<= 1;
        }
        let bpp = if vbe { r.vbe_regs[crate::vga::dispi::INDEX_BPP as usize] } else { 0 };
        let draw = match shift_control {
            0 => Draw::Planar4 { d2 },
            1 => Draw::Cga2 { d2 },
            _ => match bpp {
                8 => Draw::Pal8,
                15 => Draw::Rgb15,
                16 => Draw::Rgb16,
                24 => Draw::Rgb24,
                32 => Draw::Rgb32,
                _ => Draw::Pal8D2,
            },
        };
        if width == 0 || height == 0 || width > 16000 || height > 12000 {
            return;
        }
        let mode = if vbe { FrameMode::Vbe } else { FrameMode::Graphics };
        resize(frame, width as u32, height as u32, mode);
        self.last_width = width as u32;
        self.last_height = height as u32;

        let pal16 = palette16(r);
        let pal256 = palette256(r);
        let plane_mask = mask16(r.ar[0x12] & 0x0f);
        let (line_offset, start_addr, line_compare) = basic_params(r);
        let mut addr1 = start_addr * 4;
        let mut y1 = 0usize;
        let mut multi_run = multi_scan;
        for y in 0..height {
            let mut addr = addr1;
            if r.cr[0x17] & 1 == 0 {
                let shift = 14 + ((r.cr[0x17] >> 6) & 1);
                addr = (addr & !(1 << shift)) | ((y1 & 1) << shift);
            }
            if r.cr[0x17] & 2 == 0 {
                addr = (addr & !0x8000) | ((y1 & 2) << 14);
            }
            let row = &mut frame.pixels[y * width..(y + 1) * width];
            draw_line(draw, vram, addr, row, &pal16, &pal256, plane_mask);
            if multi_run == 0 {
                let mask = usize::from((r.cr[0x17] & 3) ^ 3);
                if y1 & mask == mask {
                    addr1 += line_offset;
                }
                y1 += 1;
                multi_run = multi_scan;
            } else {
                multi_run -= 1;
            }
            if y == line_compare {
                addr1 = 0;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Draw {
    Planar4 { d2: bool },
    Cga2 { d2: bool },
    Pal8D2,
    Pal8,
    Rgb15,
    Rgb16,
    Rgb24,
    Rgb32,
}

fn resize(frame: &mut Frame, w: u32, h: u32, mode: FrameMode) {
    frame.width = w;
    frame.height = h;
    frame.mode = mode;
    frame.pixels.resize((w * h) as usize, 0);
}

/// Vertical display end + overflow bits 8 and 9 (without the +1).
fn vertical_display_end(r: &VgaRegs) -> usize {
    usize::from(r.cr[0x12])
        | (usize::from(r.cr[0x07] & 0x02) << 7)
        | (usize::from(r.cr[0x07] & 0x40) << 3)
}

/// QEMU `vga_get_offsets`: (line offset in VRAM bytes, start address in
/// 4-byte units, line compare).
fn basic_params(r: &VgaRegs) -> (usize, usize, usize) {
    let line_compare = usize::from(r.cr[0x18])
        | (usize::from(r.cr[0x07] & 0x10) << 4)
        | (usize::from(r.cr[0x09] & 0x40) << 3);
    if r.vbe_enabled() {
        (r.vbe_line_offset as usize, r.vbe_start_addr as usize, 65535)
    } else {
        let line_offset = usize::from(r.cr[0x13]) << 3;
        let start = usize::from(r.cr[0x0d]) | (usize::from(r.cr[0x0c]) << 8);
        (line_offset, start, line_compare)
    }
}

/// QEMU `c6_to_8`: widen a 6-bit DAC value.
fn c6_to_8(v: u8) -> u32 {
    let v = u32::from(v & 0x3f);
    let b = v & 1;
    (v << 2) | (b << 1) | b
}

fn rgb(r: u32, g: u32, b: u32) -> u32 {
    (r << 16) | (g << 8) | b
}

/// QEMU `update_palette16`: the 16 attribute palette entries through the
/// colour select register into the DAC.
fn palette16(r: &VgaRegs) -> [u32; 16] {
    let mut out = [0; 16];
    for (i, o) in out.iter_mut().enumerate() {
        let v = r.ar[i];
        let v = if r.ar[0x10] & 0x80 != 0 {
            ((r.ar[0x14] & 0x0f) << 4) | (v & 0x0f)
        } else {
            ((r.ar[0x14] & 0x0c) << 4) | (v & 0x3f)
        };
        let p = usize::from(v) * 3;
        *o = rgb(c6_to_8(r.palette[p]), c6_to_8(r.palette[p + 1]), c6_to_8(r.palette[p + 2]));
    }
    out
}

/// QEMU `update_palette256`.
fn palette256(r: &VgaRegs) -> [u32; 256] {
    let mut out = [0; 256];
    for (i, o) in out.iter_mut().enumerate() {
        let c = &r.palette[i * 3..i * 3 + 3];
        *o = if r.dac_8bit {
            rgb(c[0].into(), c[1].into(), c[2].into())
        } else {
            rgb(c6_to_8(c[0]), c6_to_8(c[1]), c6_to_8(c[2]))
        };
    }
    out
}

fn mask16(i: u8) -> u32 {
    (0..4).filter(|p| i & (1 << p) != 0).map(|p| 0xffu32 << (p * 8)).sum()
}

/// One glyph row (`bits`, MSB = leftmost) into `dst`: 8, 9 or 16 pixels.
fn draw_glyph_row(dst: &mut [u32], bits: u8, cw: usize, fg: u32, bg: u32, dup9: bool) {
    let px = |i: u32| if bits & (0x80 >> i) != 0 { fg } else { bg };
    match cw {
        16 => {
            for i in 0..8 {
                dst[2 * i as usize] = px(i);
                dst[2 * i as usize + 1] = px(i);
            }
        }
        9 => {
            for i in 0..8 {
                dst[i as usize] = px(i);
            }
            dst[8] = if dup9 { px(7) } else { bg };
        }
        _ => {
            for i in 0..8 {
                dst[i as usize] = px(i);
            }
        }
    }
}

/// The four plane bytes at interleaved address `addr` (QEMU
/// `vga_read_dword_le`, which wraps within VRAM).
fn dword(vram: &crate::vga::Vram, addr: usize) -> u32 {
    let a = addr & (VRAM_SIZE - 1) & !3;
    vram.latch_at(a >> 2)
}

fn byte(vram: &crate::vga::Vram, addr: usize) -> u8 {
    vram.get(addr & (VRAM_SIZE - 1))
}

fn draw_line(
    draw: Draw,
    vram: &crate::vga::Vram,
    mut addr: usize,
    row: &mut [u32],
    pal16: &[u32; 16],
    pal256: &[u32; 256],
    plane_mask: u32,
) {
    let width = row.len();
    match draw {
        Draw::Planar4 { d2 } => {
            let rep = if d2 { 2 } else { 1 };
            for group in 0..width / (8 * rep) {
                let data = dword(vram, addr) & plane_mask;
                for i in 0..8 {
                    let bit = 7 - i;
                    let c = (0..4).map(|p| ((data >> (p * 8 + bit)) & 1) << p).sum::<u32>();
                    for k in 0..rep {
                        row[(group * 8 + i as usize) * rep + k] = pal16[c as usize];
                    }
                }
                addr += 4;
            }
        }
        Draw::Cga2 { d2 } => {
            // QEMU vga_draw_line2: 2 bits per pixel. Pixels 0-3 take their
            // bit pairs (MSB pair first) from planes 0 (low) and 2 (high),
            // pixels 4-7 from planes 1 and 3.
            let rep = if d2 { 2 } else { 1 };
            for group in 0..width / (8 * rep) {
                let data = dword(vram, addr) & plane_mask;
                let plane = |p: u32| (data >> (p * 8)) & 0xff;
                for k in 0..8 {
                    let (lo, hi) = if k < 4 { (plane(0), plane(2)) } else { (plane(1), plane(3)) };
                    let shift = 2 * (3 - k % 4);
                    let c = ((lo >> shift) & 3) | (((hi >> shift) & 3) << 2);
                    for j in 0..rep {
                        row[(group * 8 + k as usize) * rep + j] = pal16[c as usize];
                    }
                }
                addr += 4;
            }
        }
        Draw::Pal8D2 => {
            for (x, px) in row.iter_mut().enumerate() {
                *px = pal256[usize::from(byte(vram, addr + x / 2))];
            }
        }
        Draw::Pal8 => {
            for (x, px) in row.iter_mut().enumerate() {
                *px = pal256[usize::from(byte(vram, addr + x))];
            }
        }
        Draw::Rgb15 | Draw::Rgb16 => {
            for (x, px) in row.iter_mut().enumerate() {
                let v = u32::from(byte(vram, addr + 2 * x)) | u32::from(byte(vram, addr + 2 * x + 1)) << 8;
                // QEMU vga_draw_line15_le / 16_le: no low-bit replication.
                *px = if matches!(draw, Draw::Rgb15) {
                    rgb((v >> 7) & 0xf8, (v >> 2) & 0xf8, (v << 3) & 0xf8)
                } else {
                    rgb((v >> 8) & 0xf8, (v >> 3) & 0xfc, (v << 3) & 0xf8)
                };
            }
        }
        Draw::Rgb24 => {
            for (x, px) in row.iter_mut().enumerate() {
                let a = addr + 3 * x;
                *px = rgb(byte(vram, a + 2).into(), byte(vram, a + 1).into(), byte(vram, a).into());
            }
        }
        Draw::Rgb32 => {
            for (x, px) in row.iter_mut().enumerate() {
                let a = addr + 4 * x;
                *px = rgb(byte(vram, a + 2).into(), byte(vram, a + 1).into(), byte(vram, a).into());
            }
        }
    }
}

/// The text screen as characters (plane 0 from the start address), for
/// headless runs: `rows` lines of `cols` characters.
pub fn text_screen(vga: &Vga) -> Vec<String> {
    let r = vga.regs();
    let cols = usize::from(r.cr[0x01]) + 1;
    let cheight = usize::from(r.cr[0x09] & 0x1f) + 1;
    let rows = ((vertical_display_end(r) + 1) / cheight).clamp(1, 100);
    let (line_offset, start, _) = basic_params(r);
    let per_line = (line_offset / 4).max(cols);
    (0..rows)
        .map(|y| {
            (0..cols)
                .map(|x| match vga.vram().plane(0)[(start + y * per_line + x) % (VRAM_SIZE / 4)] {
                    c @ 0x20..=0x7e => c as char,
                    _ => ' ',
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vga::dispi;

    fn vga() -> Vga {
        Vga::new(&[0x55, 0xaa, 0x01])
    }

    /// Program a standard VGA mode from its register table (the layout of
    /// SeaVGABIOS's stdvga mode tables), then turn the display on (PAS).
    fn set_mode(v: &mut Vga, misc: u8, seq: [u8; 4], crtc: [u8; 25], attr: [u8; 21], grdc: [u8; 9]) {
        v.io_write(0x3c2, misc);
        for (i, &x) in seq.iter().enumerate() {
            v.io_write(0x3c4, i as u8 + 1);
            v.io_write(0x3c5, x);
        }
        v.io_write(0x3d4, 0x11);
        v.io_write(0x3d5, 0);
        for (i, &x) in crtc.iter().enumerate() {
            v.io_write(0x3d4, i as u8);
            v.io_write(0x3d5, x);
        }
        v.io_read(0x3da);
        for (i, &x) in attr.iter().enumerate() {
            v.io_write(0x3c0, i as u8);
            v.io_write(0x3c0, x);
        }
        for (i, &x) in grdc.iter().enumerate() {
            v.io_write(0x3ce, i as u8);
            v.io_write(0x3cf, x);
        }
        v.io_write(0x3c0, 0x20);
    }

    /// Mode 12h (640x480x16), then TempleOS's palette setup: identity
    /// attribute palette and DAC entries 0-15 written directly.
    fn mode12(v: &mut Vga) {
        set_mode(
            v,
            0xe3,
            [0x01, 0x0f, 0x00, 0x06],
            [
                0x5f, 0x4f, 0x50, 0x82, 0x54, 0x80, 0x0b, 0x3e, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0xea, 0x8c, 0xdf, 0x28, 0x00, 0xe7, 0x04, 0xe3, 0xff,
            ],
            [
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x14, 0x07, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
                0x01, 0x00, 0x0f, 0x00, 0x00,
            ],
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x0f, 0xff],
        );
        // GrPalette.HC: attribute registers 0-15 = i, then PAS.
        v.io_read(0x3da);
        for i in 0..16u8 {
            v.io_write(0x3c0, i);
            v.io_write(0x3c0, i);
        }
        v.io_write(0x3c0, 0x20);
        v.io_write(0x3c0, 0);
        v.io_read(0x3da);
        for i in 0..16u8 {
            v.io_write(0x3c8, i);
            v.io_write(0x3c9, i * 4); // r
            v.io_write(0x3c9, 0x3f - i); // g
            v.io_write(0x3c9, i); // b
        }
    }

    fn color(i: u8) -> u32 {
        rgb(c6_to_8(i * 4), c6_to_8(0x3f - i), c6_to_8(i))
    }

    #[test]
    fn c6_to_8_matches_qemu() {
        assert_eq!(c6_to_8(0), 0);
        assert_eq!(c6_to_8(0x3f), 0xff);
        // Only bit 0 is replicated into the new low bits.
        assert_eq!(c6_to_8(0x2a), 0xa8);
        assert_eq!(c6_to_8(0x15), 0x57);
    }

    #[test]
    fn mode12_planar_pixels() {
        let mut v = vga();
        mode12(&mut v);
        // TempleOS's GrUpdateVGAGraphics: one plane at a time via the map mask.
        // Pixel 0 = colour 0b0101, pixel 7 = 0b1010, second row pixel 8 = 15.
        for (p, byte0) in [(0u8, 0x80u8), (1, 0x01), (2, 0x80), (3, 0x01)] {
            v.io_write(0x3c4, 0x02);
            v.io_write(0x3c5, 1 << p);
            v.mem_write(0, byte0);
            v.mem_write(80 + 1, 0x80);
        }
        let mut f = Frame::default();
        Renderer::new().render(&v, 0, &mut f);
        assert_eq!((f.width, f.height, f.mode), (640, 480, FrameMode::Graphics));
        assert_eq!(f.pixels[0], color(0b0101));
        assert_eq!(f.pixels[7], color(0b1010));
        assert_eq!(f.pixels[1], color(0));
        assert_eq!(f.pixels[640 + 8], color(15));
        assert_eq!(f.pixels[640 * 479 + 639], color(0));

        // The attribute controller's plane enable masks planes out.
        v.io_read(0x3da);
        v.io_write(0x3c0, 0x12);
        v.io_write(0x3c0, 0x03);
        v.io_write(0x3c0, 0x20);
        Renderer::new().render(&v, 0, &mut f);
        assert_eq!(f.pixels[0], color(0b0001));

        // Start address scrolls by bytes.
        v.io_write(0x3d4, 0x0d);
        v.io_write(0x3d5, 80);
        Renderer::new().render(&v, 0, &mut f);
        assert_eq!(f.pixels[8], color(0b0011));
    }

    #[test]
    fn blank_until_palette_address_source_set() {
        let mut v = vga();
        mode12(&mut v);
        v.io_read(0x3da);
        v.io_write(0x3c0, 0x00); // PAS clear: screen blank
        let mut f = Frame::default();
        let mut r = Renderer::new();
        r.render(&v, 0, &mut f);
        assert_eq!((f.width, f.height, f.mode), (640, 480, FrameMode::Blank));
        assert!(f.pixels.iter().all(|&p| p == 0));
    }

    fn mode3(v: &mut Vga) {
        set_mode(
            v,
            0x67,
            [0x00, 0x03, 0x00, 0x02],
            [
                0x5f, 0x4f, 0x50, 0x82, 0x55, 0x81, 0xbf, 0x1f, 0x00, 0x4f, 0x0d, 0x0e, 0x00, 0x00, 0x00, 0x00,
                0x9c, 0x8e, 0x8f, 0x28, 0x1f, 0x96, 0xb9, 0xa3, 0xff,
            ],
            [
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x14, 0x07, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
                0x0c, 0x00, 0x0f, 0x08, 0x00,
            ],
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x0e, 0x0f, 0xff],
        );
        // All 64 DAC entries the mode 3 attribute palette can select.
        v.io_write(0x3c8, 0);
        for i in 0..64u8 {
            for c in [i * 4, 0x3f - i, i] {
                v.io_write(0x3c9, c);
            }
        }
    }

    #[test]
    fn mode3_text_with_font_and_cursor() {
        let mut v = vga();
        mode3(&mut v);
        // A glyph for 'A' in plane 2 (as the BIOS loads fonts: planar access
        // to plane 2), rows 0 and 15 set.
        v.vram_mut().plane_mut(2)[32 * 0x41] = 0b1000_0001;
        v.vram_mut().plane_mut(2)[32 * 0x41 + 15] = 0xff;
        v.vram_mut().plane_mut(2)[32 * 0xc4] = 0x01; // box-drawing: 9th column copies
        // Characters through the odd/even window at 0xB8000.
        let base = 0x18000;
        v.mem_write(base, b'A');
        v.mem_write(base + 1, 0x1e); // yellow on blue
        v.mem_write(base + 2, 0xc4);
        v.mem_write(base + 3, 0x07);
        v.mem_write(base + 160, b'H');
        v.mem_write(base + 162, b'i');

        let mut f = Frame::default();
        let mut r = Renderer::new();
        r.render(&v, 0, &mut f);
        assert_eq!((f.width, f.height, f.mode), (720, 400, FrameMode::Text));
        // Attribute colours go through the attribute palette (mode 3 maps
        // 0x0E to DAC entry 0x3E).
        let (fg, bg) = (color(0x3e), color(0x01));
        assert_eq!(f.pixels[0], fg);
        assert_eq!(f.pixels[1], bg);
        assert_eq!(f.pixels[7], fg);
        assert_eq!(f.pixels[8], bg, "9th column is background for letters");
        assert_eq!(f.pixels[720 * 15 + 3], fg);
        assert_eq!(f.pixels[9 + 7], color(7));
        assert_eq!(f.pixels[9 + 8], color(7), "9th column repeats for 0xB0-0xDF");

        // Cursor (CR0A=0x0d, CR0B=0x0e) at offset 1: drawn in the cell's
        // foreground on rows 13-14 while the blink phase is on.
        v.io_write(0x3d4, 0x0f);
        v.io_write(0x3d5, 1);
        r.render(&v, 1, &mut f);
        let visible = f.pixels[720 * 13 + 9 + 3] == color(7);
        r.render(&v, CURSOR_PERIOD_NS, &mut f);
        assert_ne!(f.pixels[720 * 13 + 9 + 3] == color(7), visible, "cursor blinks");

        assert_eq!(&text_screen(&v)[..2], ["A", "Hi"]);
    }

    #[test]
    fn mode13_chain4() {
        let mut v = vga();
        set_mode(
            &mut v,
            0x63,
            [0x01, 0x0f, 0x00, 0x0e],
            [
                0x5f, 0x4f, 0x50, 0x82, 0x54, 0x80, 0xbf, 0x1f, 0x00, 0x41, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x9c, 0x8e, 0x8f, 0x28, 0x40, 0x96, 0xb9, 0xa3, 0xff,
            ],
            [
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
                0x41, 0x00, 0x0f, 0x00, 0x00,
            ],
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x05, 0x0f, 0xff],
        );
        v.io_write(0x3c8, 200);
        for c in [0x3f, 0, 0x15] {
            v.io_write(0x3c9, c);
        }
        v.mem_write(321, 200);
        let mut f = Frame::default();
        Renderer::new().render(&v, 0, &mut f);
        // 320x200 doubled both ways: 640x400.
        assert_eq!((f.width, f.height), (640, 400));
        let red = rgb(0xff, 0, c6_to_8(0x15));
        for (x, y) in [(2, 2), (3, 2), (2, 3), (3, 3)] {
            assert_eq!(f.pixels[y * 640 + x], red, "({x},{y})");
        }
        assert_eq!(f.pixels[2 * 640 + 4], 0);
    }

    #[test]
    fn vbe_32bpp() {
        let mut v = vga();
        let set = |v: &mut Vga, i: u16, val: u16| {
            v.dispi_write(0x1ce, i);
            v.dispi_write(0x1cf, val);
        };
        set(&mut v, dispi::INDEX_XRES, 800);
        set(&mut v, dispi::INDEX_YRES, 600);
        set(&mut v, dispi::INDEX_BPP, 32);
        set(&mut v, dispi::INDEX_ENABLE, dispi::ENABLED | dispi::LFB_ENABLED);
        v.io_read(0x3da);
        v.io_write(0x3c0, 0x20);
        v.lfb_write((800 * 4 + 4) as u32, 4, 0x00_12_34_56);
        let mut f = Frame::default();
        Renderer::new().render(&v, 0, &mut f);
        assert_eq!((f.width, f.height, f.mode), (800, 600, FrameMode::Vbe));
        assert_eq!(f.pixels[800 + 1], 0x12_34_56);
        assert_eq!(f.pixels[0], 0);
    }

    fn workspace_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
    }

    /// Replay every VGA access in the reference trace (ports 0x3B0-0x3DF,
    /// the VBE ports and the 0xA0000 window) into the model, compare each
    /// read with the value QEMU returned, then compare the final picture with
    /// the run's last screenshot pixel for pixel.
    ///
    /// Needs `ref/boot/` from tools/qemu-ref/qemu_trace.py (or TEMPLEOS_TRACE
    /// pointing at a trace.log). Screenshot comparison assumes the guest
    /// didn't draw between the last screendump and the end of the trace.
    #[test]
    #[ignore]
    fn replay_vga_reference_trace() {
        use std::io::BufRead;

        let trace = std::env::var_os("TEMPLEOS_TRACE")
            .map(|p| workspace_root().join(p))
            .unwrap_or_else(|| workspace_root().join("ref/boot/trace.log"));
        let Ok(file) = std::fs::File::open(&trace) else {
            eprintln!("skipping: trace {} not found", trace.display());
            return;
        };
        let rom = std::fs::read(workspace_root().join("payload/vgabios.bin")).expect("payload/vgabios.bin");
        let mut v = Vga::new(&rom);
        let (mut reads, mut writes) = (0u64, 0u64);
        let mut mismatches = Vec::new();
        for (lineno, line) in std::io::BufReader::new(file).lines().enumerate() {
            let line = line.unwrap();
            let name = match line.rsplit_once("name '") {
                Some((_, n)) => n.trim_end_matches('\''),
                None => continue,
            };
            if !matches!(name, "vga" | "vbe" | "vga-lowmem") {
                continue;
            }
            let t: Vec<&str> = line.split_whitespace().collect();
            let is_read = match t[0] {
                "memory_region_ops_read" => true,
                "memory_region_ops_write" => false,
                _ => continue,
            };
            let num = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
            let (addr, value, size) = (num(t[6]), num(t[8]), num(t[10]) as u8);
            let got = match (name, is_read) {
                ("vga", true) => (0..size).map(|i| u64::from(v.io_read(addr as u16 + u16::from(i))) << (8 * i)).sum(),
                ("vga", false) => {
                    for i in 0..size {
                        v.io_write(addr as u16 + u16::from(i), (value >> (8 * i)) as u8);
                    }
                    value
                }
                ("vbe", true) => u64::from(v.dispi_read(addr as u16)),
                ("vbe", false) => {
                    v.dispi_write(addr as u16, value as u16);
                    value
                }
                (_, true) => u64::from(v.mem_read((addr - 0xa0000) as u32)),
                (_, false) => {
                    v.mem_write((addr - 0xa0000) as u32, value as u8);
                    value
                }
            };
            if is_read {
                reads += 1;
                if got != value {
                    mismatches.push(format!(
                        "line {}: read {name} {addr:#x} size {size}: trace {value:#x}, model {got:#x}",
                        lineno + 1
                    ));
                }
            } else {
                writes += 1;
            }
        }
        eprintln!("replay: {writes} writes, {reads} reads, {} mismatches", mismatches.len());
        for m in mismatches.iter().take(40) {
            eprintln!("  {m}");
        }
        assert!(writes > 0, "no VGA accesses in the trace");
        assert!(mismatches.is_empty(), "{} mismatching reads", mismatches.len());

        let dir = trace.parent().unwrap();
        let mut shots: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("shot-")))
            .collect();
        shots.sort();
        let Some(last) = shots.last() else {
            eprintln!("no screenshots next to the trace; skipping the picture comparison");
            return;
        };
        let want = std::fs::read(last).unwrap();
        let mut f = Frame::default();
        // In text mode the cursor's blink phase depends on QEMU's timer, so
        // accept the picture with the cursor either on or off.
        let mut r = Renderer::new();
        r.render(&v, 0, &mut f);
        let mut got = f.to_ppm();
        if got != want && f.mode == FrameMode::Text {
            r.render(&v, CURSOR_PERIOD_NS / 2, &mut f);
            got = f.to_ppm();
        }
        if got != want {
            let header_end = |b: &[u8]| b.iter().enumerate().filter(|(_, &c)| c == b'\n').nth(2).map(|(i, _)| i + 1).unwrap();
            let (g, w) = (&got[header_end(&got)..], &want[header_end(&want)..]);
            let diff = g.chunks(3).zip(w.chunks(3)).filter(|(a, b)| a != b).count();
            panic!(
                "frame differs from {}: model {}x{}, {diff} differing pixels (headers {:?} vs {:?})",
                last.display(),
                f.width,
                f.height,
                String::from_utf8_lossy(&got[..header_end(&got)]),
                String::from_utf8_lossy(&want[..header_end(&want)]),
            );
        }
        eprintln!("final frame matches {}", last.display());
    }

    fn frame(w: u32, h: u32, mode: FrameMode) -> Frame {
        Frame { width: w, height: h, pixels: vec![0; (w * h) as usize], mode }
    }

    #[test]
    fn windowed_fit_uses_whole_number_scale() {
        let f = frame(640, 480, FrameMode::Graphics);
        assert_eq!(f.fit(1400, 1000, false), (60, 20, 1280, 960));
        assert_eq!(f.fit(1279, 959, false), (319, 239, 640, 480));
        // Too small for 1x: shrink, keeping the shape.
        assert_eq!(f.fit(320, 400, false), (0, 80, 320, 240));
    }

    #[test]
    fn fullscreen_fit_is_4_3_for_vga_and_square_for_vbe() {
        assert_eq!(frame(720, 400, FrameMode::Text).fit(1920, 1080, true), (240, 0, 1440, 1080));
        assert_eq!(frame(640, 480, FrameMode::Graphics).fit(1920, 1080, true), (240, 0, 1440, 1080));
        assert_eq!(frame(800, 600, FrameMode::Vbe).fit(1920, 1200, true), (160, 0, 1600, 1200));
        assert_eq!(frame(1024, 600, FrameMode::Vbe).fit(1024, 768, true), (0, 84, 1024, 600));
    }

    #[test]
    fn ppm_matches_qemu_screendump_format() {
        let f = Frame { width: 2, height: 1, pixels: vec![0x0011_2233, 0x00ff_0000], mode: FrameMode::Graphics };
        assert_eq!(f.to_ppm(), b"P6\n2 1\n255\n\x11\x22\x33\xff\x00\x00");
    }
}
