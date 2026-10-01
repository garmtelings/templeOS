//! Run TempleOS on the software CPU without a window, on any OS: an input
//! script (devices/src/script.rs) with screenshots, or a plain boot. Waits,
//! key gaps and screenshots are in guest time and guest time is the
//! instruction count, so a run repeats exactly and matches
//! `tools/qemu-ref/qemu_trace.py --script` under -icount.
//!
//!   cargo run -p vmm --release --example softrun -- \
//!       [--iso PATH|none] [--cpus N] [--script FILE --shots DIR]
//!       [--seconds S --shot FILE.ppm] [--trace FILE]

use std::path::{Path, PathBuf};

use devices::input::InputEvent;
use devices::script::{keys_to_set2, parse, Step};
use devices::Nanos;
use vmm::{Config, SoftMachine, Stop};

/// Guest time between key and mouse events, as inputscript.py's KEY_GAP.
const GAP: Nanos = 20_000_000;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn read_static(path: &Path) -> &'static [u8] {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Box::leak(data.into_boxed_slice())
}

/// Run until guest time `t`.
fn run_to(m: &mut SoftMachine, t: Nanos) {
    match m.run_until(None, t) {
        Ok(Stop::TimeLimit) => {}
        Ok(stop) => panic!("machine stopped: {stop:?}"),
        Err(e) => panic!("{e}"),
    }
}

fn shot(m: &mut SoftMachine, path: &Path) {
    std::fs::write(path, m.render().to_ppm()).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    println!("[softrun {:8.3}s] screenshot {}", m.now() as f64 / 1e9, path.display());
}

fn main() {
    let mut iso = Some(root().join("payload/TempleOS.ISO"));
    let (mut cpus, mut script, mut shots, mut seconds, mut shot_file) = (1, None, PathBuf::from("shots"), None, None);
    let mut trace: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--iso" => iso = Some(value()).filter(|v| v != "none").map(PathBuf::from),
            "--cpus" => cpus = value().parse().expect("--cpus"),
            "--script" => script = Some(PathBuf::from(value())),
            "--shots" => shots = PathBuf::from(value()),
            "--seconds" => seconds = Some(value().parse::<f64>().expect("--seconds")),
            "--shot" => shot_file = Some(PathBuf::from(value())),
            "--trace" => trace = Some(PathBuf::from(value())),
            _ => panic!("unknown option {a}"),
        }
    }
    let mut m = SoftMachine::new(Config {
        ram_size: 1024 << 20,
        bios: read_static(&root().join("payload/bios.bin")),
        vgabios: read_static(&root().join("payload/vgabios.bin")),
        cdrom: iso.as_deref().map(read_static),
        hdd: None,
        rtc_base: 1_511_136_000, // qemu_trace.py: -rtc base=2017-11-20T00:00:00
        vga_fast_path: false,
        cpus,
        exact_time: true,
    })
    .unwrap_or_else(|e| panic!("{e}"));
    m.set_debugcon_sink(Box::new(std::io::sink()), false);
    if let Some(t) = &trace {
        let f = std::fs::File::create(t).unwrap_or_else(|e| panic!("{}: {e}", t.display()));
        m.set_trace(Box::new(std::io::BufWriter::new(f)));
    }

    let host = std::time::Instant::now();
    if let Some(script) = script {
        let steps = parse(&std::fs::read_to_string(&script).expect("script")).expect("script syntax");
        std::fs::create_dir_all(&shots).expect("shots dir");
        for step in steps {
            let now = m.now();
            match step {
                Step::Wait(s) => run_to(&mut m, now + (s.max(0.0) * 1e9) as Nanos),
                Step::Keys(events) => {
                    for (name, pressed) in events {
                        for bytes in keys_to_set2(&[(name, pressed)]) {
                            m.with_pc(|pc| pc.input(InputEvent::Key(bytes)));
                        }
                        let t = m.now() + GAP;
                        run_to(&mut m, t);
                    }
                }
                Step::Mouse { dx, dy, buttons } => {
                    // Scripts use screen coordinates (y down); PS/2's y is up.
                    m.with_pc(|pc| pc.input(InputEvent::Mouse { dx, dy: -dy, dz: 0, buttons }));
                    run_to(&mut m, now + GAP);
                }
                Step::Screenshot(name) => shot(&mut m, &shots.join(format!("{name}.ppm"))),
            }
        }
    } else {
        run_to(&mut m, (seconds.unwrap_or(60.0) * 1e9) as Nanos);
        if let Some(f) = shot_file {
            shot(&mut m, &f);
        }
    }
    let guest = m.now() as f64 / 1e9;
    let wall = host.elapsed().as_secs_f64();
    println!("[softrun] {guest:.1}s of guest time in {wall:.1}s; milestones {:?}", m.reached());
}
