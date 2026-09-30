//! The script path's keyboard bytes against QEMU's: run ref/input/keys.script
//! (every printable character typed, every other key pressed alone; see
//! tools/qemu-ref/input_ref.py) through devices::script, the host key
//! translator and the board's i8042 with translation on, as SeaBIOS sets it,
//! and compare the bytes read from port 0x60 with the ones SeaBIOS read in
//! the QEMU run. `cargo test -p devices --test replay_input -- --ignored`.

use std::io::BufRead;
use std::path::PathBuf;

use devices::input::InputEvent;
use devices::pc::{Pc, PcConfig};
use devices::script::{keys_to_set2, parse, Step};
use devices::GuestMemory;

struct NoRam;

impl GuestMemory for NoRam {
    fn read(&self, _: u64, _: &mut [u8]) -> bool {
        false
    }
    fn write(&mut self, _: u64, _: &[u8]) -> bool {
        false
    }
}

#[test]
#[ignore]
fn script_keys_match_qemu() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let dir = root.join("ref/input");
    let (Ok(script), Ok(trace)) =
        (std::fs::read_to_string(dir.join("keys.script")), std::fs::File::open(dir.join("trace.log")))
    else {
        eprintln!("skipping: run tools/qemu-ref/input_ref.py first");
        return;
    };

    // What SeaBIOS read from port 0x60 in QEMU.
    let qemu: Vec<u8> = std::io::BufReader::new(trace)
        .lines()
        .map(|l| l.unwrap())
        .filter(|l| l.starts_with("memory_region_ops_read") && l.ends_with("name 'i8042-data'"))
        .map(|l| u8::from_str_radix(l.split_whitespace().nth(8).unwrap().trim_start_matches("0x"), 16).unwrap())
        .collect();

    // The same keys through our path.
    static ROM: [u8; 3] = [0x55, 0xaa, 0x01];
    let mut pc = Pc::new(PcConfig { ram_size: 512 << 20, vgabios: &ROM, cdrom: None, hdd: None, rtc_base: 0, cpus: 1 });
    pc.io_write(0x64, 1, 0x60, 0, &mut NoRam);
    pc.io_write(0x60, 1, 0x61, 0, &mut NoRam); // translate, system flag, kbd IRQ
    let mut ours = Vec::new();
    for step in parse(&script).expect("keys.script") {
        let Step::Keys(events) = step else { continue };
        for bytes in keys_to_set2(&events) {
            pc.input(InputEvent::Key(bytes));
            while pc.io_read(0x64, 1, 0) & 1 != 0 {
                ours.push(pc.io_read(0x60, 1, 0) as u8);
            }
        }
    }

    if let Ok(dump) = std::env::var("TEMPLEOS_DUMP") {
        std::fs::write(dump, ours.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")).unwrap();
    }
    assert!(!ours.is_empty());
    eprintln!("{} bytes from the script, {} port 0x60 reads in the QEMU run", ours.len(), qemu.len());
    let tail = &qemu[qemu.len().saturating_sub(ours.len())..];
    if tail != ours.as_slice() {
        let first = tail.iter().zip(&ours).position(|(a, b)| a != b).unwrap_or(0);
        panic!(
            "byte streams differ at {first}: qemu {:02x?}, ours {:02x?}",
            &tail[first..(first + 12).min(tail.len())],
            &ours[first..(first + 12).min(ours.len())]
        );
    }
    eprintln!("all {} bytes match", ours.len());
}
