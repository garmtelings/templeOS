//! TempleOS.exe: boots the embedded, unmodified TempleOS V5.03 ISO in a
//! purpose-built VMM on the Windows Hypervisor Platform (see PLAN.md).
//!
//! Phases 1-2 are headless: the machine runs, reports boot milestones and the
//! SeaBIOS debug console, and can stop at a milestone.

use std::fs::File;
use std::io::BufWriter;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vmm::{Config, Machine, Milestone, Stop};

static BIOS: &[u8] = include_bytes!("../payload/bios.bin");
static VGABIOS: &[u8] = include_bytes!("../payload/vgabios.bin");
static ISO: &[u8] = include_bytes!("../payload/TempleOS.ISO");

const USAGE: &str = "\
usage: templeos [options]

  --mem MIB           guest RAM in MiB (default 1024, minimum 512)
  --until MILESTONE   stop at bios-banner, long-mode or kernel-timers
  --seconds N         stop after N seconds of wall time
  --iso PATH          boot this ISO instead of the embedded one
  --no-cd             boot with no CD in the drive
  --rtc-base UNIX     guest clock at power-on, Unix seconds (default: now)
  --debugcon FILE     also write the SeaBIOS debug console to FILE
  --trace FILE        log every port/MMIO access in QEMU trace format
  --screen            print the VGA text screen when stopping
  --quiet             don't echo the debug console
";

struct Args {
    mem_mib: u64,
    until: Option<Milestone>,
    seconds: Option<f64>,
    iso: Option<String>,
    no_cd: bool,
    rtc_base: Option<i64>,
    debugcon: Option<String>,
    trace: Option<String>,
    screen: bool,
    quiet: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        mem_mib: 1024,
        until: None,
        seconds: None,
        iso: None,
        no_cd: false,
        rtc_base: None,
        debugcon: None,
        trace: None,
        screen: false,
        quiet: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--mem" => a.mem_mib = value()?.parse().map_err(|_| "bad --mem")?,
            "--until" => {
                a.until = Some(match value()?.as_str() {
                    "bios-banner" => Milestone::BiosBanner,
                    "long-mode" => Milestone::LongMode,
                    "kernel-timers" => Milestone::KernelTimers,
                    other => return Err(format!("unknown milestone {other}")),
                })
            }
            "--seconds" => a.seconds = Some(value()?.parse().map_err(|_| "bad --seconds")?),
            "--iso" => a.iso = Some(value()?),
            "--no-cd" => a.no_cd = true,
            "--rtc-base" => a.rtc_base = Some(value()?.parse().map_err(|_| "bad --rtc-base")?),
            "--debugcon" => a.debugcon = Some(value()?),
            "--trace" => a.trace = Some(value()?),
            "--screen" => a.screen = true,
            "--quiet" => a.quiet = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown option {other}")),
        }
    }
    if a.mem_mib < 512 {
        return Err("TempleOS needs at least 512 MiB (--mem 512)".into());
    }
    Ok(a)
}

/// Print the faulting address and a backtrace on an access violation (or
/// any other fatal exception) instead of dying silently.
fn install_crash_handler() {
    use windows::Win32::System::Diagnostics::Debug::{AddVectoredExceptionHandler, EXCEPTION_POINTERS};
    unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
        let rec = &*(*info).ExceptionRecord;
        // Ignore first-chance exceptions that aren't fatal (e.g. C++ or debugger ones).
        if rec.ExceptionCode.0 as u32 == 0xC000_0005 || rec.ExceptionCode.0 as u32 == 0xC000_00FD {
            eprintln!(
                "templeos: fatal exception {:#x} at {:?} (info {:?})
{}",
                rec.ExceptionCode.0,
                rec.ExceptionAddress,
                &rec.ExceptionInformation[..rec.NumberParameters as usize],
                std::backtrace::Backtrace::force_capture()
            );
        }
        0 // EXCEPTION_CONTINUE_SEARCH
    }
    // SAFETY: registering a handler with a valid function pointer.
    unsafe {
        AddVectoredExceptionHandler(1, Some(handler));
    }
}

fn main() -> ExitCode {
    install_crash_handler();
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("templeos: {e}");
            }
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("templeos: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let cdrom: Option<&'static [u8]> = match (&args.iso, args.no_cd) {
        (_, true) => None,
        (Some(path), _) => Some(Vec::leak(std::fs::read(path)?)),
        (None, _) => Some(ISO),
    };
    let rtc_base = args.rtc_base.unwrap_or_else(|| {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    });
    let mut m = Machine::new(Config {
        ram_size: args.mem_mib << 20,
        bios: BIOS,
        vgabios: VGABIOS,
        cdrom,
        rtc_base,
    })?;
    if let Some(path) = &args.debugcon {
        m.set_debugcon_sink(Box::new(File::create(path)?), !args.quiet);
    } else if args.quiet {
        m.set_debugcon_sink(Box::new(std::io::sink()), false);
    }
    if let Some(path) = &args.trace {
        m.pc_mut().set_trace(Box::new(BufWriter::new(File::create(path)?)));
    }

    let limit = args.seconds.map(Duration::from_secs_f64);
    let stop = m.run(args.until, limit)?;
    match stop {
        Stop::Milestone(ms) => println!("[vmm] stopped at milestone: {}", ms.describe()),
        Stop::TimeLimit => println!("[vmm] stopped: time limit"),
        Stop::ResetRequested => println!("[vmm] stopped: guest requested a reset"),
    }
    if args.screen {
        print_text_screen(m.memory().ram());
    }
    let unhandled = devices::unhandled_report();
    if !unhandled.is_empty() {
        println!("[vmm] unhandled accesses:");
        for (kind, addr, n) in unhandled.iter().take(20) {
            println!("  {kind} {addr:#x}: {n}");
        }
    }
    Ok(())
}

/// The 80x25 text screen at 0xB8000 (what SeaBIOS and the TempleOS boot
/// loader print before the kernel switches to graphics).
fn print_text_screen(ram: &[u8]) {
    let screen = &ram[0xB8000..0xB8000 + 80 * 25 * 2];
    println!("[vmm] text screen:");
    for row in screen.chunks(160) {
        let line: String = row
            .chunks(2)
            .map(|c| match c[0] {
                0x20..=0x7E => c[0] as char,
                _ => ' ',
            })
            .collect();
        println!("  |{}|", line.trim_end());
    }
}
