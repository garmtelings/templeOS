//! Fuzz the whole board: random port and MMIO accesses, passing time, host
//! input, interrupt acknowledges and rendering, as a misbehaving guest could
//! produce them. The board must never panic (that would kill the VMM) and
//! must keep answering.
//!
//! `cargo test -p devices --test fuzz_board` runs a fixed set of seeds.
//! For a longer run: `TEMPLEOS_FUZZ_ITERS=5000000 TEMPLEOS_FUZZ_SEED=7 cargo
//! test -p devices --release --test fuzz_board -- --nocapture`.
//! A failure prints the seed and the last operations, so it can be replayed.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};

use devices::ide::MemDisk;
use devices::input::InputEvent;
use devices::pc::{Pc, PcConfig};
use devices::vga_render::{Frame, Renderer};
use devices::GuestMemory;

/// xorshift64*: small, fast, reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// Guest RAM for fw_cfg DMA: 16 MiB, reads past it fail as they would.
struct Ram(Vec<u8>);

impl GuestMemory for Ram {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        let end = gpa.saturating_add(buf.len() as u64);
        if end > self.0.len() as u64 {
            return false;
        }
        buf.copy_from_slice(&self.0[gpa as usize..end as usize]);
        true
    }
    fn write(&mut self, gpa: u64, data: &[u8]) -> bool {
        let end = gpa.saturating_add(data.len() as u64);
        if end > self.0.len() as u64 {
            return false;
        }
        self.0[gpa as usize..end as usize].copy_from_slice(data);
        true
    }
}

/// Ports worth hitting: every device the board has, plus neighbours.
const PORTS: &[u16] = &[
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x20, 0x21, 0xa0, 0xa1, 0x4d0, 0x4d1, 0x40, 0x41, 0x42, 0x43, 0x61, 0x60, 0x64, 0x70, 0x71,
    0x80, 0x81, 0x82, 0x83, 0x87, 0x89, 0x8a, 0x8b, 0x8f, 0x92, 0xb2, 0xb3, 0xc0, 0xc4, 0xc8, 0xcc,
    0xd0, 0xd2, 0xd4, 0xd6, 0xd8, 0xda, 0xdc, 0xde, 0x1f0, 0x1f1, 0x1f2, 0x1f3, 0x1f4, 0x1f5,
    0x1f6, 0x1f7, 0x3f6, 0x3f7, 0x170, 0x171, 0x172, 0x173, 0x174, 0x175, 0x176, 0x177, 0x376,
    0xcf8, 0xcf9, 0xcfa, 0xcfb, 0xcfc, 0xcfd, 0xcfe, 0xcff, 0x510, 0x511, 0x514, 0x518, 0x1ce,
    0x1cf, 0x1d0, 0x3b4, 0x3b5, 0x3ba, 0x3c0, 0x3c1, 0x3c2, 0x3c4, 0x3c5, 0x3c6, 0x3c7, 0x3c8,
    0x3c9, 0x3ca, 0x3cc, 0x3ce, 0x3cf, 0x3d4, 0x3d5, 0x3da, 0x402, 0x600, 0x604, 0x608, 0xb000,
    0xb004, 0xb008, 0xc000, 0xc040,
];

const IDE_COMMANDS: &[u8] = &[
    0x00, 0x08, 0x10, 0x20, 0x21, 0x24, 0x25, 0x27, 0x29, 0x30, 0x31, 0x34, 0x35, 0x37, 0x39, 0x3c,
    0x40, 0x41, 0x42, 0x70, 0x90, 0x91, 0x98, 0xa0, 0xa1, 0xb0, 0xc4, 0xc5, 0xc6, 0xc8, 0xca, 0xe0,
    0xe1, 0xe5, 0xe7, 0xea, 0xec, 0xef, 0xf8, 0xf9,
];

fn value(r: &mut Rng, size: u8) -> u32 {
    let v = match r.below(6) {
        0 => 0,
        1 => u32::MAX,
        2 => r.below(16) as u32,
        3 => 1 << r.below(32),
        _ => r.next() as u32,
    };
    if size >= 4 {
        v
    } else {
        v & ((1 << (8 * size)) - 1)
    }
}

fn run(seed: u64, iters: u64) {
    let mut rng = Rng(seed | 1);
    let rom: &'static [u8] = Box::leak(vec![0x55u8, 0xaa, 0x40, 0xcb].into_boxed_slice());
    // A small "CD": 64 sectors of 2048 bytes, with a volume descriptor.
    let mut iso = vec![0u8; 64 * 2048];
    iso[16 * 2048..16 * 2048 + 6].copy_from_slice(b"\x01CD001");
    let iso: &'static [u8] = Box::leak(iso.into_boxed_slice());
    let mut pc = Pc::new(PcConfig {
        ram_size: 512 << 20,
        vgabios: rom,
        cdrom: Some(iso),
        hdd: Some(Box::new(MemDisk(vec![0; 4 << 20]))),
        rtc_base: 1_511_136_000,
        cpus: 2,
    });
    let mut ram = Ram(vec![0; 16 << 20]);
    let mut renderer = Renderer::new();
    let mut frame = Frame::default();
    let mut now: u64 = 0;
    let mut log: VecDeque<String> = VecDeque::new();

    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut slowest = (std::time::Duration::ZERO, String::new());
        for i in 0..iters {
            let t0 = std::time::Instant::now();
            let op = rng.below(100);
            let entry = match op {
                0..=39 => {
                    let size = rng.pick(&[1u8, 1, 1, 2, 4]);
                    let port = if rng.chance(90) { rng.pick(PORTS) } else { rng.below(0x10000) as u16 };
                    let val = if (port == 0x1f7 || port == 0x177) && rng.chance(70) {
                        rng.pick(IDE_COMMANDS).into()
                    } else if port == 0xcf8 && size == 4 && rng.chance(70) {
                        0x8000_0000 | (rng.below(4) as u32) << 11 | (rng.below(8) as u32) << 8 | (rng.below(64) as u32) << 2
                    } else {
                        value(&mut rng, size)
                    };
                    pc.io_write(port, size, val, now, &mut ram);
                    format!("{i}: out {port:#x} size {size} {val:#x}")
                }
                40..=64 => {
                    let size = rng.pick(&[1u8, 1, 1, 2, 4]);
                    let port = if rng.chance(90) { rng.pick(PORTS) } else { rng.below(0x10000) as u16 };
                    pc.io_read(port, size, now);
                    format!("{i}: in {port:#x} size {size}")
                }
                65..=79 => {
                    let size = rng.pick(&[1u8, 2, 4, 8]);
                    let gpa = match rng.below(6) {
                        0 | 1 => 0xa0000 + rng.below(0x20000),
                        2 => 0xfed0_0000 + rng.below(0x400),
                        3 => 0xfd00_0000 + rng.below(16 << 20),
                        4 => 0xfebf_0000 + rng.below(0x20000),
                        _ => rng.next() & 0xffff_ffff,
                    };
                    let val = rng.next();
                    if rng.chance(60) {
                        pc.mmio_write(gpa, size, val, now);
                        format!("{i}: mmio write {gpa:#x} size {size} {val:#x}")
                    } else {
                        pc.mmio_read(gpa, size, now);
                        format!("{i}: mmio read {gpa:#x} size {size}")
                    }
                }
                80..=87 => {
                    now += match rng.below(4) {
                        0 => rng.below(1_000),
                        1 => rng.below(1_000_000),
                        2 => rng.below(100_000_000),
                        _ => rng.below(10_000_000_000),
                    };
                    pc.poll(now);
                    let _ = pc.next_deadline();
                    format!("{i}: poll {now}")
                }
                88..=92 => {
                    let mut n = 0;
                    while pc.has_interrupt() && n < 20 {
                        pc.acknowledge_interrupt();
                        n += 1;
                    }
                    format!("{i}: ack x{n}")
                }
                93..=94 => {
                    // A whole transaction, so deep device states get reached.
                    match rng.below(3) {
                        0 => {
                            // IDE: select, task file, command, then a data phase.
                            let (base, ctl) = if rng.chance(50) { (0x1f0u16, 0x3f6u16) } else { (0x170, 0x376) };
                            let dev = rng.pick(&[0xa0u8, 0xb0, 0xe0, 0xf0, 0xef]);
                            pc.io_write(ctl, 1, rng.pick(&[0x08u32, 0x00, 0x80, 0x02]), now, &mut ram);
                            pc.io_write(base + 6, 1, dev.into(), now, &mut ram);
                            for reg in 1..=5 {
                                pc.io_write(base + reg, 1, value(&mut rng, 1), now, &mut ram);
                                if rng.chance(30) {
                                    pc.io_write(base + reg, 1, value(&mut rng, 1), now, &mut ram);
                                }
                            }
                            let cmd = rng.pick(IDE_COMMANDS);
                            pc.io_write(base + 7, 1, cmd.into(), now, &mut ram);
                            let words = rng.pick(&[6u32, 256, 512, 2048, 8192]);
                            let write = rng.chance(50);
                            for _ in 0..words {
                                if write {
                                    let v = value(&mut rng, 2);
                                    pc.io_write(base, 2, v, now, &mut ram);
                                } else {
                                    pc.io_read(base, 2, now);
                                }
                                if rng.chance(2) {
                                    pc.io_read(base + 7, 1, now);
                                }
                            }
                            format!("{i}: ide burst base {base:#x} dev {dev:#x} cmd {cmd:#x} {words} words write={write}")
                        }
                        1 => {
                            // PS/2: controller command, device command via D4.
                            let n = rng.below(12);
                            for _ in 0..n {
                                let c = rng.pick(&[0x20u32, 0x60, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xad, 0xae, 0xd0, 0xd1, 0xd2, 0xd3, 0xd4, 0xfe, 0xf0]);
                                pc.io_write(0x64, 1, c, now, &mut ram);
                                pc.io_write(0x60, 1, value(&mut rng, 1), now, &mut ram);
                                for _ in 0..rng.below(4) {
                                    pc.io_read(0x60, 1, now);
                                }
                            }
                            format!("{i}: ps2 burst x{n}")
                        }
                        _ => {
                            // VGA: a few register writes, then a burst into the window.
                            for _ in 0..rng.below(8) {
                                let (idx, data) = rng.pick(&[(0x3c4u16, 0x3c5u16), (0x3ce, 0x3cf), (0x3d4, 0x3d5)]);
                                pc.io_write(idx, 1, rng.below(0x20) as u32, now, &mut ram);
                                pc.io_write(data, 1, value(&mut rng, 1), now, &mut ram);
                            }
                            let base = 0xa0000 + rng.below(0x20000);
                            let n = rng.below(4096);
                            for k in 0..n {
                                let gpa = 0xa0000 + (base + k * 8 - 0xa0000) % 0x20000;
                                if rng.chance(80) {
                                    pc.mmio_write(gpa, 8, rng.next(), now);
                                } else {
                                    pc.mmio_read(gpa, 8, now);
                                }
                            }
                            format!("{i}: vga burst {n} qwords at {base:#x}")
                        }
                    }
                }
                95..=97 => {
                    let ev = if rng.chance(50) {
                        let len = rng.below(9) as usize;
                        InputEvent::Key((0..len).map(|_| rng.next() as u8).collect())
                    } else {
                        InputEvent::Mouse {
                            dx: (rng.next() as i32) >> rng.below(31),
                            dy: (rng.next() as i32) >> rng.below(31),
                            dz: (rng.next() as i32) >> rng.below(31),
                            buttons: rng.next() as u8,
                        }
                    };
                    let s = format!("{i}: input {ev:?}");
                    pc.input(ev);
                    s
                }
                _ => {
                    renderer.render(pc.vga(), now, &mut frame);
                    let _ = pc.speaker_hz();
                    let _ = pc.take_debugcon();
                    let _ = pc.take_reset_request();
                    let _ = pc.vga_fast_plane();
                    format!("{i}: render {}x{}", frame.width, frame.height)
                }
            };
            let took = t0.elapsed();
            if took > slowest.0 {
                slowest = (took, entry.clone());
            }
            // A guest must not be able to stall the host: debug builds are
            // ~20x slower than release, so this is generous.
            assert!(took < std::time::Duration::from_secs(2), "operation took {took:?}: {entry}");
            if log.len() == 40 {
                log.pop_front();
            }
            log.push_back(entry);
        }
        if std::env::var_os("TEMPLEOS_FUZZ_VERBOSE").is_some() {
            eprintln!("seed {seed}: slowest operation {:?}: {}", slowest.0, slowest.1);
        }
    }));
    if let Err(e) = result {
        let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
        panic!(
            "board panicked with seed {seed}: {}\nlast operations:\n  {}",
            msg.unwrap_or_default(),
            log.iter().cloned().collect::<Vec<_>>().join("\n  ")
        );
    }
}

#[test]
fn board_survives_random_guests() {
    let iters: u64 = std::env::var("TEMPLEOS_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(40_000);
    let seeds: Vec<u64> = match std::env::var("TEMPLEOS_FUZZ_SEED").ok().and_then(|v| v.parse().ok()) {
        Some(s) => vec![s],
        None => (1..=8).collect(),
    };
    for seed in seeds {
        run(seed, iters);
    }
}
