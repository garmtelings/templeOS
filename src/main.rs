//! TempleOS.exe: boots the embedded, unmodified TempleOS V5.03 ISO in a
//! purpose-built VMM on the Windows Hypervisor Platform, or, where that
//! can't be used, on a software CPU (Bochs's; see PLAN.md).
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

use vmm::{Config, Machine, Milestone, SoftMachine, Stop};

static BIOS: &[u8] = include_bytes!("../payload/bios.bin");
/// SHA-256 of each embedded file as pinned in payload/ (see build.rs).
static PINS: [(&str, &[u8], &str); 3] = [
    ("SeaBIOS", BIOS, env!("PAYLOAD_SHA256_BIOS")),
    ("SeaVGABIOS", VGABIOS, env!("PAYLOAD_SHA256_VGABIOS")),
    ("TempleOS.ISO", ISO, env!("PAYLOAD_SHA256_TEMPLEOS")),
];
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
  --cpus N            CPU cores (default: with a window, the host's cores
                      up to 8; headless, 1)
  --until MILESTONE   stop at bios-banner, long-mode, kernel-timers or
                      ap-started
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
  --script FILE       run an input script (type, key, mouse, wait,
                      screenshot; see devices/src/script.rs), then stop
  --shots DIR         where the script's screenshots go (default: shots)
  --debugcon FILE     also write the SeaBIOS debug console to FILE
  --trace FILE        log every port/MMIO access in QEMU trace format
                      (implies --exact-vga)
  --screen            print the VGA text screen when stopping
  --quiet             don't echo the debug console
  --software-cpu      run on the software CPU even if the Windows Hypervisor
                      Platform is available (it is used without it)
  --hypervisor        require the Windows Hypervisor Platform
";

struct Args {
    script: Option<Vec<devices::script::Step>>,
    shots: String,
    cpus: Option<u32>,
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
    backend: Backend,
}

/// Which CPU runs the guest.
#[derive(Clone, Copy, PartialEq)]
enum Backend {
    /// The hypervisor if it's available, else the software CPU.
    Auto,
    Hypervisor,
    Software,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        script: None,
        shots: "shots".into(),
        cpus: None,
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
        backend: Backend::Auto,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--headless" => a.headless = true,
            "--hdd" => a.hdd = Some(value()?),
            "--script" => {
                let path = value()?;
                let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
                a.script = Some(devices::script::parse(&text).map_err(|e| format!("{path}: {e}"))?);
            }
            "--shots" => a.shots = value()?,
            "--cpus" => {
                let n: u32 = value()?.parse().map_err(|_| "bad --cpus")?;
                if !(1..=vmm::machine::MAX_CPUS).contains(&n) {
                    return Err(format!("--cpus must be 1 to {}", vmm::machine::MAX_CPUS));
                }
                a.cpus = Some(n);
            }
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
                    "ap-started" => Milestone::ApStarted,
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
            "--software-cpu" => a.backend = Backend::Software,
            "--hypervisor" => a.backend = Backend::Hypervisor,
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
    if let Err(e) = verify_payload() {
        if args.headless {
            eprintln!("templeos: {e}");
        } else {
            window::error_box(&e);
        }
        return ExitCode::FAILURE;
    }
    let result = if args.headless { run_headless(&args) } else { run_windowed(args) };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("templeos: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Check the embedded BIOS, VGA BIOS and ISO against the hashes pinned at
/// build time, so a damaged or modified exe never boots something other
/// than the official image.
fn verify_payload() -> Result<(), String> {
    use sha2::{Digest, Sha256};
    for (name, data, want) in PINS {
        let got: String = Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect();
        if got != want {
            return Err(format!(
                "this TempleOS.exe is damaged: its embedded {name} has SHA-256 {got}, not the pinned {want}"
            ));
        }
    }
    Ok(())
}

/// The guest's machine: on the hypervisor or on the software CPU, which
/// offer the same interface.
enum Vm {
    Hypervisor(Machine),
    Software(SoftMachine),
}

macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        impl Vm {
            $(fn $name(&mut self, $($arg: $ty),*) -> $ret {
                match self {
                    Vm::Hypervisor(m) => m.$name($($arg),*),
                    Vm::Software(m) => m.$name($($arg),*),
                }
            })*
        }
    };
}

forward! {
    set_debugcon_sink(w: Box<dyn std::io::Write>, echo: bool) -> ();
    set_trace(w: Box<dyn std::io::Write + Send>) -> ();
    set_display(f: vmm::types::DisplayFn) -> ();
    set_input(q: Arc<devices::input::InputQueue>) -> ();
    set_stop_flag(f: Arc<std::sync::atomic::AtomicBool>) -> ();
    set_speaker(f: vmm::types::SpeakerFn) -> ();
    run(until: Option<Milestone>, limit: Option<Duration>) -> Result<Stop, vmm::Error>;
}

impl Vm {
    fn render(&mut self) -> &devices::vga_render::Frame {
        match self {
            Vm::Hypervisor(m) => m.render(),
            Vm::Software(m) => m.render(),
        }
    }

    fn with_pc<R>(&self, f: impl FnOnce(&mut devices::pc::Pc) -> R) -> R {
        match self {
            Vm::Hypervisor(m) => m.with_pc(f),
            Vm::Software(m) => m.with_pc(f),
        }
    }
}

fn machine(args: &Args) -> Result<Vm, Box<dyn std::error::Error>> {
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
    let software = match args.backend {
        Backend::Software => true,
        Backend::Hypervisor => false,
        Backend::Auto => match vmm::whpx::check_available() {
            Ok(()) => false,
            Err(why) => {
                // Said once per run, not on every guest reboot.
                static SAID: std::sync::Once = std::sync::Once::new();
                SAID.call_once(|| {
                    let why = why.lines().next().unwrap_or_default();
                    println!("[vmm] {why} Using the software CPU (slower).");
                });
                true
            }
        },
    };
    let cfg = Config {
        ram_size: args.mem_mib << 20,
        bios: BIOS,
        vgabios: VGABIOS,
        cdrom,
        hdd,
        rtc_base,
        vga_fast_path: !args.exact_vga,
        // The software CPU runs every core on one host thread, so more cores
        // only make it slower; it uses one unless asked.
        cpus: args.cpus.unwrap_or_else(|| {
            if args.headless || software {
                1
            } else {
                std::thread::available_parallelism().map_or(1, |n| n.get() as u32).min(8)
            }
        }),
        exact_time: false,
    };
    let mut m = if software { Vm::Software(SoftMachine::new(cfg)?) } else { Vm::Hypervisor(Machine::new(cfg)?) };
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

/// Run an input script against a running machine: keys and mouse go to
/// `input` (20 ms apart, as tools/qemu-ref/inputscript.py paces QEMU),
/// screenshots are taken from `frame()`. Sets `stop` when done.
fn spawn_script(
    steps: Vec<devices::script::Step>,
    input: Arc<devices::input::InputQueue>,
    frame: impl Fn() -> devices::vga_render::Frame + Send + 'static,
    stop: Arc<std::sync::atomic::AtomicBool>,
    shots: String,
) -> std::thread::JoinHandle<()> {
    use devices::input::InputEvent;
    use devices::script::Step;
    const GAP: Duration = Duration::from_millis(20);
    std::thread::spawn(move || {
        let _ = std::fs::create_dir_all(&shots);
        for step in steps {
            if stop.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            match step {
                Step::Wait(s) => std::thread::sleep(Duration::from_secs_f64(s.max(0.0))),
                Step::Keys(events) => {
                    for (name, pressed) in events {
                        let bytes = devices::script::keys_to_set2(&[(name, pressed)]);
                        for b in bytes {
                            input.push(InputEvent::Key(b));
                        }
                        std::thread::sleep(GAP);
                    }
                }
                Step::Mouse { dx, dy, buttons } => {
                    // Scripts use screen coordinates (y down); PS/2's y is up.
                    input.push(InputEvent::Mouse { dx, dy: -dy, dz: 0, buttons });
                    std::thread::sleep(GAP);
                }
                Step::Screenshot(name) => {
                    let path = std::path::Path::new(&shots).join(format!("{name}.ppm"));
                    match std::fs::write(&path, frame().to_ppm()) {
                        Ok(()) => println!("[script] screenshot {}", path.display()),
                        Err(e) => eprintln!("[script] {}: {e}", path.display()),
                    }
                }
            }
        }
        println!("[script] done");
        stop.store(true, std::sync::atomic::Ordering::Release);
    })
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
    let scripted = args.script.is_some();
    shared.unattended.store(scripted, std::sync::atomic::Ordering::Relaxed);
    let script = args.script.clone().map(|steps| {
        let s = shared.clone();
        spawn_script(steps, shared.input.clone(), move || s.latest_frame(), shared.stop.clone(), args.shots.clone())
    });
    let started = window::run(shared.clone(), args.fullscreen, || {
        vm_thread = Some(
            std::thread::Builder::new()
                .name("vcpu".into())
                .spawn(move || {
                    let error = vm_thread_main(&args, &vm_shared, &vm_speaker).err().map(|e| e.to_string());
                    if let Some(e) = &error {
                        eprintln!("[vmm] error: {e}");
                    }
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
    if let Some(t) = script {
        let _ = t.join();
    }
    if let Err(e) = started {
        window::error_box(&e);
        return Err(e.into());
    }
    match shared.exit_error() {
        Some(e) if scripted => Err(e.into()),
        _ => Ok(()),
    }
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
    let _script = args.script.clone().map(|steps| {
        let latest = Arc::new(std::sync::Mutex::new(devices::vga_render::Frame::default()));
        let store = latest.clone();
        m.set_display(Box::new(move |f| store.lock().unwrap().clone_from(f)));
        let input = Arc::new(devices::input::InputQueue::new());
        m.set_input(input.clone());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        m.set_stop_flag(stop.clone());
        spawn_script(steps, input, move || latest.lock().unwrap().clone(), stop, args.shots.clone())
    });
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
        for line in m.with_pc(|pc| devices::vga_render::text_screen(pc.vga())) {
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
