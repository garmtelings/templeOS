//! The software machine boots the pinned SeaBIOS and, when the ISO is
//! there, TempleOS: the same milestones the hypervisor machine reports.
//!
//! Without payload/TempleOS.ISO only the BIOS part runs. Run with
//! `cargo test -p vmm --release --test soft_boot -- --nocapture`.

use std::path::PathBuf;
use std::time::Duration;

use vmm::{Config, Milestone, SoftMachine, Stop};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn payload(name: &str) -> Option<&'static [u8]> {
    std::fs::read(root().join("payload").join(name)).ok().map(|v| &*Box::leak(v.into_boxed_slice()))
}

fn config(cdrom: Option<&'static [u8]>, cpus: u32) -> Config {
    Config {
        ram_size: 1024 << 20,
        bios: payload("bios.bin").expect("payload/bios.bin"),
        vgabios: payload("vgabios.bin").expect("payload/vgabios.bin"),
        cdrom,
        hdd: None,
        rtc_base: 1_511_136_000,
        vga_fast_path: false,
        cpus,
        exact_time: true,
    }
}

#[test]
fn seabios_runs_without_media() {
    let mut m = SoftMachine::new(config(None, 1)).unwrap();
    let stop = m.run(Some(Milestone::BiosBanner), Some(Duration::from_secs(60))).unwrap();
    assert!(matches!(stop, Stop::Milestone(Milestone::BiosBanner)), "{stop:?}");
    // With nothing to boot SeaBIOS says so on the screen.
    let found = (0..200).any(|_| {
        m.run(None, Some(Duration::from_millis(100))).unwrap();
        m.with_pc(|pc| devices::vga_render::text_screen(pc.vga())).iter().any(|l| l.contains("No bootable device"))
    });
    let screen = m.with_pc(|pc| devices::vga_render::text_screen(pc.vga()));
    assert!(found, "screen:\n{}", screen.join("\n"));
}

#[test]
fn templeos_boots_from_cd() {
    let Some(iso) = payload("TempleOS.ISO") else {
        eprintln!("skipping: payload/TempleOS.ISO not found (tools/fetch-payload.sh)");
        return;
    };
    let host = std::time::Instant::now();
    let mut m = SoftMachine::new(config(Some(iso), 1)).unwrap();
    let stop = m.run_until(Some(Milestone::KernelTimers), 120_000_000_000).unwrap();
    assert!(matches!(stop, Stop::Milestone(Milestone::KernelTimers)), "{stop:?}, reached {:?}", m.reached());
    assert!(m.reached().contains(&Milestone::LongMode));
    eprintln!(
        "kernel timers at {:.2}s of guest time, after {:.1}s",
        m.now() as f64 / 1e9,
        host.elapsed().as_secs_f64()
    );
    // By 60 s the CD boot shows its install prompt in 640x480 graphics.
    assert!(matches!(m.run_until(None, 60_000_000_000).unwrap(), Stop::TimeLimit));
    let frame = m.render();
    assert_eq!(frame.mode, devices::vga_render::FrameMode::Graphics);
    let mut colors: Vec<u32> = frame.pixels.clone();
    colors.sort_unstable();
    colors.dedup();
    assert!(colors.len() > 2, "a blank screen");
    eprintln!("guest at 60 s after {:.1}s", host.elapsed().as_secs_f64());
}
