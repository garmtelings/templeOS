//! TempleOS.exe: boots the embedded, unmodified TempleOS V5.03 ISO in a
//! purpose-built VMM on the Windows Hypervisor Platform (see PLAN.md).
//!
//! By default it opens a window showing the VGA display. `--headless` runs
//! without one: the machine reports boot milestones and the SeaBIOS debug
//! console, and can stop at a milestone.

mod audio;
mod window;

use std::fs::File;
use std::io::BufWriter;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vmm::{Config, Machine, Milestone, Stop};

static BIOS: &[u8] = include_bytes!("../payload/bios.bin");
static VGABIOS: &[u8] = include_bytes!("../payload/vgabios.bin");
static ISO: &[u8] = include_bytes!("../payload/TempleOS.ISO");

const USAGE: &str = "\
usage: templeos [options]

  --headless          run without a window
  --fullscreen        start fullscreen

In the window: click to capture the mouse. Right Ctrl, or Ctrl+Alt pressed
and released together, releases it. Right Ctrl+F or Ctrl+Alt+Enter toggles
fullscreen. All other keys go to TempleOS.
  --mem MIB           guest RAM in MiB (default 1024, minimum 512)
  --until MILESTONE   stop at bios-banner, long-mode or kernel-timers
  --seconds N         stop after N seconds of wall time
  --iso PATH          boot this ISO instead of the embedded one
  --no-cd             boot with no CD in the drive
  --hdd PATH          hard disk image (raw; created, 2 GiB, if missing).
                      Default with a window: TempleOS.hdd next to the exe
                      (or in %LOCALAPPDATA%\\TempleOS); headless: none
  --no-hdd            no hard disk
  --rtc-base UNIX     guest clock at power-on, Unix seconds (default: now)
  --exact-vga         emulate every VGA memory access (no plane mapping)
  --screenshot FILE   save the display as a PPM when stopping
  --debugcon FILE     also write the SeaBIOS debug console to FILE
  --trace FILE        log every port/MMIO access in QEMU trace format
                      (implies --exact-vga)
  --screen            print the VGA text screen when stopping
  --quiet             don't echo the debug console
";

struct Args {
    hdd: Option<String>,
    no_hdd: bool,
    headless: bool,
    fullscreen: bool,
    exact_vga: bool,
    screenshot: Option<String>,
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
        hdd: None,
        no_hdd: false,
        headless: false,
        fullscreen: false,
        exact_vga: false,
        screenshot: None,
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
            "--headless" => a.headless = true,
            "--hdd" => a.hdd = Some(value()?),
            "--no-hdd" => a.no_hdd = true,
            "--fullscreen" => a.fullscreen = true,
            "--exact-vga" => a.exact_vga = true,
            "--screenshot" => a.screenshot = Some(value()?),
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
    let result = if args.headless { run_headless(&args) } else { run_windowed(args) };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("templeos: {e}");
            ExitCode::FAILURE
        }
    }
}

fn machine(args: &Args) -> Result<Machine, Box<dyn std::error::Error>> {
    // Read an --iso file once, however often the guest reboots.
    static ISO_FILE: std::sync::OnceLock<&'static [u8]> = std::sync::OnceLock::new();
    let cdrom: Option<&'static [u8]> = match (&args.iso, args.no_cd) {
        (_, true) => None,
        (Some(path), _) => match ISO_FILE.get() {
            Some(iso) => Some(*iso),
            None => Some(*ISO_FILE.get_or_init(|| Vec::leak(std::fs::read(path).unwrap_or_default()))),
        },
        (None, _) => Some(ISO),
    };
    if cdrom.is_some_and(|c| c.is_empty()) {
        return Err(format!("cannot read {}", args.iso.as_deref().unwrap_or_default()).into());
    }
    let rtc_base = args.rtc_base.unwrap_or_else(|| {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    });
    let hdd = match hdd_path(args) {
        Some(path) => {
            let disk = devices::ide::FileDisk::open(&path, HDD_SIZE)
                .map_err(|e| format!("hard disk {}: {e}", path.display()))?;
            Some(Box::new(disk) as Box<dyn devices::ide::DiskImage>)
        }
        None => None,
    };
    let mut m = Machine::new(Config {
        ram_size: args.mem_mib << 20,
        bios: BIOS,
        vgabios: VGABIOS,
        cdrom,
        hdd,
        rtc_base,
        vga_fast_path: !args.exact_vga,
    })?;
    if let Some(path) = &args.debugcon {
        m.set_debugcon_sink(Box::new(File::create(path)?), !args.quiet);
    } else if args.quiet {
        m.set_debugcon_sink(Box::new(std::io::sink()), false);
    }
    if let Some(path) = &args.trace {
        m.set_trace(Box::new(BufWriter::new(File::create(path)?)));
    }
    Ok(m)
}

/// Size of a newly created hard disk image.
const HDD_SIZE: u64 = 2 << 30;

/// Which hard disk image to use: --hdd, else (with a window) TempleOS.hdd
/// next to the exe, or under %LOCALAPPDATA%\TempleOS when the exe's folder
/// isn't writable.
fn hdd_path(args: &Args) -> Option<std::path::PathBuf> {
    if args.no_hdd {
        return None;
    }
    if let Some(p) = &args.hdd {
        return Some(p.into());
    }
    if args.headless {
        return None;
    }
    let beside_exe = std::env::current_exe().ok()?.with_file_name("TempleOS.hdd");
    let writable = |p: &std::path::Path| {
        p.exists() || std::fs::OpenOptions::new().write(true).create_new(true).open(p).map(|_| std::fs::remove_file(p)).is_ok()
    };
    if writable(&beside_exe) {
        return Some(beside_exe);
    }
    let dir = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("TempleOS");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("TempleOS.hdd"))
}

/// The normal way to run: a window, with the VM on its own thread. A guest
/// reboot restarts the machine; closing the window stops it.
fn run_windowed(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    hide_console_if_ours();
    let shared = window::Shared::new();
    let speaker = audio::Speaker::new();
    let audio_thread = audio::start(speaker.clone());
    let vm_shared = shared.clone();
    let vm_speaker = speaker.clone();
    let mut vm_thread = None;
    let started = window::run(shared.clone(), args.fullscreen, || {
        vm_thread = Some(
            std::thread::Builder::new()
                .name("vcpu".into())
                .spawn(move || {
                    let error = vm_thread_main(&args, &vm_shared, &vm_speaker).err().map(|e| e.to_string());
                    vm_shared.vm_exited(error);
                })
                .expect("spawn VM thread"),
        );
    });
    shared.stop.store(true, std::sync::atomic::Ordering::Release);
    if let Some(t) = vm_thread {
        let _ = t.join();
    }
    speaker.stop();
    let _ = audio_thread.join();
    started.map_err(|e| {
        window::error_box(&e);
        e.into()
    })
}

fn vm_thread_main(
    args: &Args,
    shared: &Arc<window::Shared>,
    speaker: &Arc<audio::Speaker>,
) -> Result<(), Box<dyn std::error::Error>> {
    let limit = args.seconds.map(Duration::from_secs_f64);
    loop {
        let mut m = machine(args)?;
        let s = shared.clone();
        m.set_display(Box::new(move |frame| s.present(frame)));
        m.set_stop_flag(shared.stop.clone());
        m.set_input(shared.input.clone());
        let sp = speaker.clone();
        m.set_speaker(Box::new(move |hz| sp.set(hz)));
        let stop = m.run(args.until, limit)?;
        if let Some(path) = &args.screenshot {
            std::fs::write(path, m.render().to_ppm())?;
        }
        speaker.set(None);
        match stop {
            Stop::ResetRequested => println!("[vmm] guest reset: rebooting"),
            _ => return Ok(()),
        }
    }
}

/// Started by double-click, the console window is ours alone: close it so
/// only the TempleOS window shows. From a terminal it stays for the logs.
fn hide_console_if_ours() {
    use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut pids = [0u32; 2];
    // SAFETY: the buffer is valid for its length.
    unsafe {
        if GetConsoleProcessList(&mut pids) == 1 {
            let _ = FreeConsole();
        }
    }
}

fn run_headless(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let mut m = machine(args)?;
    let limit = args.seconds.map(Duration::from_secs_f64);
    let stop = m.run(args.until, limit)?;
    match stop {
        Stop::Milestone(ms) => println!("[vmm] stopped at milestone: {}", ms.describe()),
        Stop::TimeLimit => println!("[vmm] stopped: time limit"),
        Stop::ResetRequested => println!("[vmm] stopped: guest requested a reset"),
        Stop::Quit => println!("[vmm] stopped"),
    }
    if let Some(path) = &args.screenshot {
        std::fs::write(path, m.render().to_ppm())?;
        println!("[vmm] screenshot written to {path}");
    }
    if args.screen {
        println!("[vmm] text screen:");
        for line in devices::vga_render::text_screen(m.pc_mut().vga()) {
            println!("  |{line}|");
        }
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
