//! Replay a whole QEMU reference trace into the whole board: every port and
//! MMIO access the guest made, in order, through the board's own routing.
//! Every read whose value doesn't depend on time must match QEMU's.
//!
//! The trace has no timestamps, so the board runs at time 0 and reads that
//! depend on time are executed but not compared:
//! - PIT counters, port 0x61 bits 4-5 (refresh toggle, channel 2 output);
//! - RTC time registers, status A (update in progress) and C (flags);
//! - the HPET main counter and the ACPI PM timer;
//! - the PICs' IRR/ISR (port 0x20/0xA0 reads: they reflect timer interrupts);
//! - fw_cfg data once the guest has used DMA (DMA moves data through guest
//!   RAM, which the trace doesn't contain, so the selector state after it
//!   can't be reproduced);
//! - IDE status reads that saw BSY in QEMU while the model is idle (QEMU
//!   does disk I/O asynchronously; the model completes it at once).
//!
//! Local APIC accesses (0xFEE00000) are left out altogether: the hypervisor
//! emulates the local APIC, so they never reach the board.
//!
//! Needs a trace (`TEMPLEOS_TRACE`, default `ref/boot/trace.log`) and the
//! media the run had: `TEMPLEOS_CD` (path, or `none`; default the pinned
//! ISO) and `TEMPLEOS_HDD` (raw image the run started from). Run with
//! `cargo test -p devices --test replay_board -- --ignored --nocapture`.

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::PathBuf;

use devices::ide::{DiskImage, MemDisk};
use devices::pc::{Pc, PcConfig};
use devices::GuestMemory;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// Guest RAM stand-in for fw_cfg DMA: empty, so a DMA descriptor reads as
/// zeros (the control word 0 does nothing).
struct NoRam;

impl GuestMemory for NoRam {
    fn read(&self, _: u64, buf: &mut [u8]) -> bool {
        buf.fill(0);
        true
    }
    fn write(&mut self, _: u64, _: &[u8]) -> bool {
        true
    }
}

#[derive(Default)]
struct Stats {
    compared: u64,
    skipped: u64,
    writes: u64,
}

#[test]
#[ignore]
fn replay_whole_board() {
    let env_path = |name: &str| std::env::var_os(name).map(|p| workspace_root().join(p));
    let trace = env_path("TEMPLEOS_TRACE").unwrap_or_else(|| workspace_root().join("ref/boot/trace.log"));
    let Ok(file) = std::fs::File::open(&trace) else {
        eprintln!("skipping: trace {} not found", trace.display());
        return;
    };
    let cdrom: Option<&'static [u8]> = match std::env::var("TEMPLEOS_CD").ok().as_deref() {
        Some("none") => None,
        Some(p) => Some(Box::leak(std::fs::read(workspace_root().join(p)).expect("TEMPLEOS_CD").into_boxed_slice())),
        None => match std::fs::read(workspace_root().join("payload/TempleOS.ISO")) {
            Ok(iso) => Some(Box::leak(iso.into_boxed_slice())),
            Err(_) => {
                eprintln!("skipping: payload/TempleOS.ISO not found (set TEMPLEOS_CD=none for a run without a CD)");
                return;
            }
        },
    };
    let hdd = env_path("TEMPLEOS_HDD")
        .map(|p| Box::new(MemDisk(std::fs::read(&p).expect("TEMPLEOS_HDD"))) as Box<dyn DiskImage>);
    let rom: &'static [u8] =
        Box::leak(std::fs::read(workspace_root().join("payload/vgabios.bin")).expect("vgabios").into_boxed_slice());
    let mut pc = Pc::new(PcConfig {
        ram_size: 1024 << 20,
        vgabios: rom,
        cdrom,
        hdd,
        rtc_base: 1_511_136_000, // qemu_trace.py: -rtc base=2017-11-20T00:00:00
        cpus: 1,
    });

    let mut stats: BTreeMap<String, Stats> = BTreeMap::new();
    let mut mismatches = Vec::new();
    let mut rtc_index = 0u8;
    let mut fwcfg_dma_used = false;
    let mut accesses = 0u64;
    for (lineno, line) in std::io::BufReader::new(file).lines().enumerate() {
        let line = line.unwrap();
        let t: Vec<&str> = line.split_whitespace().collect();
        let is_read = match t.first() {
            Some(&"memory_region_ops_read") => true,
            Some(&"memory_region_ops_write") => false,
            _ => continue,
        };
        // "cpu -1": a device's own access (MSI), not the guest's.
        if t[2] == "-1" {
            continue;
        }
        // The local APIC is emulated by the hypervisor, not the board.
        if t[6].starts_with("0xfee") && t[6].len() == 10 {
            continue;
        }
        let num = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
        let (addr, value, size) = (num(t[6]), num(t[8]), num(t[10]) as u8);
        // QEMU logs the handler's value before cutting it to the access size.
        let value = if size >= 8 { value } else { value & ((1 << (8 * size)) - 1) };
        let name = line.rsplit_once("name '").map(|(_, n)| n.trim_end_matches('\'')).unwrap_or("?").to_string();
        accesses += 1;
        let port = addr < 0x10000;
        if !is_read {
            if port {
                if addr == 0x70 {
                    rtc_index = value as u8 & 0x7f;
                }
                if (0x514..=0x51b).contains(&addr) {
                    fwcfg_dma_used = true;
                }
                pc.io_write(addr as u16, size, value as u32, 0, &mut NoRam);
            } else {
                pc.mmio_write(addr, size, value, 0);
            }
            stats.entry(name).or_default().writes += 1;
            continue;
        }
        let got = if port { u64::from(pc.io_read(addr as u16, size, 0)) } else { pc.mmio_read(addr, size, 0) };
        // Which bits of this read are independent of time.
        let mask: u64 = match (port, addr) {
            (true, 0x40..=0x42) => 0,
            (true, 0x61) => !0x30,
            (true, 0x71) if matches!(rtc_index, 0x00..=0x09 | 0x0a | 0x0c | 0x32) => 0,
            (true, 0x20 | 0xa0) => 0,
            (true, 0x510..=0x51b) if fwcfg_dma_used => 0,
            (true, p) if name == "acpi-tmr" || (0x600..=0x63f).contains(&p) && p & 0x3c == 0x08 => 0,
            (false, a) if (0xfed0_00f0..0xfed0_00f8).contains(&a) => 0,
            (true, 0x1f7 | 0x177 | 0x3f6 | 0x376) if value & 0x80 != 0 && got & 0x80 == 0 => 0,
            _ => u64::MAX,
        };
        let s = stats.entry(name.clone()).or_default();
        if mask == 0 {
            s.skipped += 1;
            continue;
        }
        s.compared += 1;
        if got & mask != value & mask {
            mismatches.push(format!(
                "line {}: read {name} {addr:#x} size {size}: qemu {value:#x}, model {got:#x}",
                lineno + 1
            ));
        }
    }

    eprintln!("replayed {accesses} accesses from {}", trace.display());
    eprintln!("{:<22} {:>9} {:>9} {:>9}", "region", "writes", "compared", "skipped");
    for (name, s) in &stats {
        eprintln!("{name:<22} {:>9} {:>9} {:>9}", s.writes, s.compared, s.skipped);
    }
    eprintln!("{} mismatching reads", mismatches.len());
    for m in mismatches.iter().take(60) {
        eprintln!("  {m}");
    }
    assert!(accesses > 0, "no accesses in the trace");
    assert!(mismatches.is_empty(), "{} mismatching reads", mismatches.len());
}
