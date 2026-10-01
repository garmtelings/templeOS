//! The machine on the software CPU: Bochs's x86-64 core (the `softcpu`
//! crate) running the same board as the hypervisor machine, for PCs where
//! the Windows Hypervisor Platform can't be used. It runs on any OS, so it
//! also boots TempleOS in tests on Linux.
//!
//! All processors run on the caller's thread, taking turns a trace at a time
//! (Bochs's SMP loop). The board is called from inside the core, through the
//! callbacks below; between runs of the core the loop handles input, the
//! display and the board's news, as [`crate::machine`] does between exits.
//!
//! Time: the core counts ticks, one instruction on each processor, and a
//! tick is a nanosecond of guest time. With [`Config::exact_time`] that is
//! all, and a run repeats exactly. Otherwise the guest clock is kept with
//! the host's between runs: a guest ahead of it waits, one behind (busy, or
//! slower than real time) skips forward, as time passes for a real CPU.
//!
//! Memory is laid out as in [`crate::memory`] (RAM, with the BIOS's top
//! 128 KiB copied to 0xE0000; the BIOS flash below 4 GiB; the VGA window and
//! device registers emulated by the board).

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use devices::input::InputQueue;
use devices::pc::{Pc, PcConfig};
use devices::vga_render::{Frame, Renderer};
use devices::Nanos;
use softcpu::*;

use crate::cpuid::Cpuid;
use crate::events::{BoardNews, Events};
use crate::types::*;

const BIOS_SIZE: usize = 256 << 10;
const MAX_LOW_RAM: u64 = 0xE000_0000;
/// The VGA window, never RAM.
const VGA_HOLE: std::ops::Range<u64> = devices::vga::WINDOW_BASE..devices::vga::WINDOW_BASE + devices::vga::WINDOW_SIZE;
/// Guest time the core runs before the loop looks at input, the display
/// and the clock again.
const SLICE: Nanos = 1_000_000;

/// Bochs keeps its processors in globals: one machine at a time.
static ONE_MACHINE: Mutex<()> = Mutex::new(());

/// Zeroed host memory on a 4 KiB boundary: the core's TLB forms host
/// addresses by OR-ing the page offset onto a page's host address.
struct PageBuf {
    ptr: *mut u8,
    len: usize,
}

impl PageBuf {
    fn new(len: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(len, 4096).expect("guest memory size");
        // SAFETY: len is non-zero (checked by the callers); failure is handled below.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        PageBuf { ptr, len }
    }
}

impl std::ops::Deref for PageBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr..ptr+len is a live allocation owned by self.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl std::ops::DerefMut for PageBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, with exclusive access through &mut self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for PageBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in new with this layout.
        unsafe { std::alloc::dealloc(self.ptr, std::alloc::Layout::from_size_align(self.len, 4096).unwrap()) };
    }
}

/// What the core's callbacks work on.
struct Board {
    pc: Pc,
    ram: PageBuf,
    bios: PageBuf,
    cpuid: Cpuid,
    /// The core's tick count at power-on (ticks keep counting across
    /// machines; guest time starts at 0 for each).
    tick0: u64,
    /// The board timer the core is armed for, in guest time.
    armed: Option<Nanos>,
    log_level: c_int,
}

impl Board {
    fn now(&self) -> Nanos {
        // SAFETY: plain query of the core's tick count.
        unsafe { softcpu_ticks() }.saturating_sub(self.tick0)
    }

    /// After anything that can change the 8259's output or the board's next
    /// timer: tell the core.
    fn sync(&mut self) {
        // SAFETY: plain calls into the core; it is initialized while a Board exists.
        unsafe { softcpu_set_intr(self.pc.has_interrupt() as c_int) };
        let next = self.pc.next_deadline();
        if next != self.armed {
            self.armed = next;
            if let Some(t) = next {
                unsafe { softcpu_set_deadline(self.tick0 + t) };
            }
        }
    }
}

/// Guest RAM as fw_cfg DMA sees it.
struct Ram<'a>(&'a mut [u8]);

impl Ram<'_> {
    fn range(&self, gpa: u64, len: usize) -> Option<std::ops::Range<usize>> {
        let end = gpa.checked_add(len as u64)?;
        let ram = gpa >= VGA_HOLE.end || end <= VGA_HOLE.start;
        (ram && end <= self.0.len() as u64).then_some(gpa as usize..end as usize)
    }
}

impl devices::GuestMemory for Ram<'_> {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        match self.range(gpa, buf.len()) {
            Some(r) => {
                buf.copy_from_slice(&self.0[r]);
                true
            }
            None => false,
        }
    }

    fn write(&mut self, gpa: u64, data: &[u8]) -> bool {
        match self.range(gpa, data.len()) {
            Some(r) => {
                self.0[r].copy_from_slice(data);
                // SAFETY: plain call into the core.
                unsafe { softcpu_mem_written(gpa, data.len() as u64) };
                true
            }
            None => false,
        }
    }
}

// The core's callbacks. `ctx` is the machine's RefCell<Board>, which
// outlives the core's use of it (see SoftMachine::drop); the core calls
// back only from inside softcpu_run and softcpu_advance, when the loop
// holds no borrow of the board.

unsafe fn board<'a>(ctx: *mut c_void) -> std::cell::RefMut<'a, Board> {
    (*(ctx as *const RefCell<Board>)).borrow_mut()
}

unsafe extern "C" fn cb_io_read(ctx: *mut c_void, port: u16, len: u32) -> u32 {
    let mut b = board(ctx);
    let now = b.now();
    let v = b.pc.io_read(port, len as u8, now);
    b.sync();
    v
}

unsafe extern "C" fn cb_io_write(ctx: *mut c_void, port: u16, len: u32, value: u32) {
    let mut b = board(ctx);
    let now = b.now();
    let Board { pc, ram, .. } = &mut *b;
    pc.io_write(port, len as u8, value, now, &mut Ram(ram));
    b.sync();
}

unsafe extern "C" fn cb_mmio_read(ctx: *mut c_void, addr: u64, len: u32) -> u64 {
    let mut b = board(ctx);
    let now = b.now();
    let v = b.pc.mmio_read(addr, len as u8, now);
    b.sync();
    v
}

unsafe extern "C" fn cb_mmio_write(ctx: *mut c_void, addr: u64, len: u32, value: u64) {
    let mut b = board(ctx);
    let now = b.now();
    b.pc.mmio_write(addr, len as u8, value, now);
    b.sync();
}

unsafe extern "C" fn cb_iac(ctx: *mut c_void) -> u8 {
    let mut b = board(ctx);
    let v = b.pc.acknowledge_interrupt();
    b.sync();
    v
}

unsafe extern "C" fn cb_cpuid(ctx: *mut c_void, cpu: u32, leaf: u32, subleaf: u32, out: *mut u32) {
    let r = board(ctx).cpuid.query(leaf, subleaf, cpu as u8);
    for (i, v) in [r.eax, r.ebx, r.ecx, r.edx].into_iter().enumerate() {
        *out.add(i) = v;
    }
}

unsafe extern "C" fn cb_deadline(ctx: *mut c_void) {
    let mut b = board(ctx);
    let now = b.now();
    b.pc.poll(now);
    b.armed = None;
    b.sync();
}

unsafe extern "C" fn cb_log(ctx: *mut c_void, level: c_int, msg: *const c_char) {
    if level >= board(ctx).log_level {
        eprintln!("{}", CStr::from_ptr(msg).to_string_lossy());
    }
}

pub struct SoftMachine {
    board: Box<RefCell<Board>>,
    cpus: u32,
    exact_time: bool,
    start: Instant,
    events: Events,
    renderer: Renderer,
    frame: Frame,
    next_frame: Nanos,
    display: Option<DisplayFn>,
    stop_flag: Option<Arc<AtomicBool>>,
    input: Option<Arc<InputQueue>>,
    _one: MutexGuard<'static, ()>,
}

impl SoftMachine {
    pub fn new(cfg: Config) -> Result<Self, Error> {
        if cfg.bios.len() != BIOS_SIZE {
            return Err(Error(format!("BIOS image must be {BIOS_SIZE} bytes")));
        }
        if !(1 << 20..=MAX_LOW_RAM).contains(&cfg.ram_size) || !cfg.ram_size.is_multiple_of(4096) {
            return Err(Error(format!("unsupported RAM size {:#x}", cfg.ram_size)));
        }
        let one = ONE_MACHINE.lock().unwrap_or_else(|e| e.into_inner());
        let cpus = cfg.cpus.clamp(1, MAX_CPUS);
        let mut ram = PageBuf::new(cfg.ram_size as usize);
        ram[0xE0000..0x100000].copy_from_slice(&cfg.bios[BIOS_SIZE - 0x20000..]);
        let mut bios = PageBuf::new(BIOS_SIZE);
        bios.copy_from_slice(cfg.bios);
        let pc = Pc::new(PcConfig {
            ram_size: cfg.ram_size,
            vgabios: cfg.vgabios,
            cdrom: cfg.cdrom,
            hdd: cfg.hdd,
            rtc_base: cfg.rtc_base,
            cpus: cpus as u8,
        });
        let log_level = match std::env::var("TEMPLEOS_DEBUG").as_deref() {
            Ok("bochs") => 0,
            Ok(_) => 2,
            Err(_) => 3,
        };
        let board = Box::new(RefCell::new(Board {
            pc,
            ram,
            bios,
            cpuid: Cpuid::qemu64_smp(cpus),
            tick0: 0,
            armed: None,
            log_level,
        }));
        let host = Host {
            ctx: &*board as *const RefCell<Board> as *mut c_void,
            io_read: cb_io_read,
            io_write: cb_io_write,
            mmio_read: cb_mmio_read,
            mmio_write: cb_mmio_write,
            iac: cb_iac,
            cpuid: cb_cpuid,
            deadline: cb_deadline,
            log: cb_log,
        };
        let (ram_ptr, bios_ptr) = {
            let b = board.borrow();
            (b.ram.ptr, b.bios.ptr)
        };
        // SAFETY: RAM and the BIOS stay at these addresses for the machine's
        // life (page-aligned allocations owned by the board); the core
        // copies `host`.
        let r = unsafe { softcpu_init(&host, cpus, ram_ptr, cfg.ram_size, bios_ptr, BIOS_SIZE as u32) };
        if r != 0 {
            return Err(core_error());
        }
        {
            let mut b = board.borrow_mut();
            // SAFETY: plain query.
            b.tick0 = unsafe { softcpu_ticks() };
            b.sync();
        }
        let start = Instant::now();
        Ok(SoftMachine {
            board,
            cpus,
            exact_time: cfg.exact_time,
            start,
            events: Events::new(start),
            renderer: Renderer::new(),
            frame: Frame::default(),
            next_frame: 0,
            display: None,
            stop_flag: None,
            input: None,
            _one: one,
        })
    }

    /// Also copy the raw debug console (port 0x402) output to `w`.
    pub fn set_debugcon_sink(&mut self, w: Box<dyn std::io::Write>, echo: bool) {
        self.events.set_debugcon_sink(w, echo);
    }

    /// Run `f` on the board.
    pub fn with_pc<R>(&self, f: impl FnOnce(&mut Pc) -> R) -> R {
        f(&mut self.board.borrow_mut().pc)
    }

    /// Log every port and MMIO access (see [`Pc::set_trace`]).
    pub fn set_trace(&mut self, w: Box<dyn std::io::Write + Send>) {
        self.board.borrow_mut().pc.set_trace(w);
    }

    /// Call `f` with a freshly rendered frame every 1/60 s of guest time.
    pub fn set_display(&mut self, f: DisplayFn) {
        self.display = Some(f);
    }

    /// [`SoftMachine::run`] returns [`Stop::Quit`] soon after `flag` becomes true.
    pub fn set_stop_flag(&mut self, flag: Arc<AtomicBool>) {
        self.stop_flag = Some(flag);
    }

    /// Keyboard and mouse events from `queue` are delivered to the guest
    /// between runs of the core.
    pub fn set_input(&mut self, queue: Arc<InputQueue>) {
        self.input = Some(queue);
    }

    /// Call `f` whenever the PC speaker starts, stops or changes pitch.
    pub fn set_speaker(&mut self, f: SpeakerFn) {
        self.events.speaker = Some(f);
    }

    /// Render the display as it is now.
    pub fn render(&mut self) -> &Frame {
        let b = self.board.borrow();
        self.renderer.render(b.pc.vga(), b.now(), &mut self.frame);
        &self.frame
    }

    pub fn reached(&self) -> &[Milestone] {
        &self.events.reached
    }

    pub fn cpus(&self) -> u32 {
        self.cpus
    }

    /// Guest time since power-on.
    pub fn now(&self) -> Nanos {
        self.board.borrow().now()
    }

    /// Guest RAM.
    pub fn ram(&self) -> std::cell::Ref<'_, [u8]> {
        std::cell::Ref::map(self.board.borrow(), |b| &*b.ram)
    }

    /// Run until `until` is reached, `limit` of wall time passes, the stop
    /// flag is set, or the guest asks for a reset.
    pub fn run(&mut self, until: Option<Milestone>, limit: Option<Duration>) -> Result<Stop, Error> {
        self.run_inner(until, limit.map(|l| Instant::now() + l), None)
    }

    /// Run until `until` is reached, guest time reaches `t` (then returns
    /// [`Stop::TimeLimit`], at `t` to within one trace of instructions), the
    /// stop flag is set, or the guest asks for a reset.
    pub fn run_until(&mut self, until: Option<Milestone>, t: Nanos) -> Result<Stop, Error> {
        self.run_inner(until, None, Some(t))
    }

    fn run_inner(&mut self, until: Option<Milestone>, deadline: Option<Instant>, guest_deadline: Option<Nanos>) -> Result<Stop, Error> {
        let debug = std::env::var_os("TEMPLEOS_DEBUG").is_some();
        let mut last_report = Instant::now();
        loop {
            let now = self.now();
            {
                let mut b = self.board.borrow_mut();
                b.pc.poll(now);
                if let Some(q) = &self.input {
                    for ev in q.take() {
                        b.pc.input(ev);
                    }
                }
                b.sync();
            }
            // SAFETY: plain queries of the core.
            if unsafe { softcpu_long_mode(0) } != 0 {
                self.events.reach(Milestone::LongMode);
            }
            let ap_started = (1..self.cpus).any(|i| unsafe { softcpu_waiting_for_sipi(i) } == 0);
            let news = BoardNews::take(&mut self.board.borrow_mut().pc);
            if let Some(stop) = self.events.handle(news, ap_started, until) {
                return Ok(stop);
            }
            if deadline.is_some_and(|d| Instant::now() >= d) || guest_deadline.is_some_and(|t| now >= t) {
                return Ok(Stop::TimeLimit);
            }
            if self.stop_flag.as_ref().is_some_and(|f| f.load(Ordering::Relaxed)) {
                return Ok(Stop::Quit);
            }
            if debug && last_report.elapsed() >= Duration::from_secs(1) {
                last_report = Instant::now();
                // SAFETY: plain queries of the core.
                let rips: Vec<String> = (0..self.cpus).map(|i| format!("{:#x}", unsafe { softcpu_rip(i) })).collect();
                eprintln!("[debug] guest {:.3}s rip={} idle={}", now as f64 / 1e9, rips.join(","), unsafe { softcpu_all_idle() });
            }
            let mut slice = SLICE;
            if self.display.is_some() {
                if now >= self.next_frame {
                    self.next_frame = now + FRAME_NS;
                    self.renderer.render(self.board.borrow().pc.vga(), now, &mut self.frame);
                    if let Some(show) = &mut self.display {
                        show(&self.frame);
                    }
                }
                slice = slice.min(self.next_frame - now);
            }
            if let Some(t) = guest_deadline {
                slice = slice.min(t - now);
            }
            // SAFETY: the core calls back into the board, which no one has borrowed now.
            if unsafe { softcpu_run(slice) } != 0 {
                return Err(core_error());
            }
            // SAFETY: plain query.
            if unsafe { softcpu_reset_requested() } != 0 {
                return Ok(Stop::ResetRequested);
            }
            if !self.exact_time {
                self.keep_time();
            }
        }
    }

    /// Keep the guest clock with the host's (see the module docs).
    fn keep_time(&mut self) {
        let guest = self.now();
        let host = self.start.elapsed().as_nanos() as Nanos;
        if guest > host {
            sleep(Duration::from_nanos(guest - host));
        } else {
            // SAFETY: between runs, as softcpu_advance requires; timers that
            // come due call back into the board, which no one has borrowed.
            unsafe { softcpu_advance(host - guest) };
        }
    }

    /// The bootstrap processor's RIP, for error reports.
    pub fn dump_regs(&self) -> String {
        // SAFETY: plain query.
        format!("rip={:#x}", unsafe { softcpu_rip(0) })
    }
}

impl Drop for SoftMachine {
    fn drop(&mut self) {
        // The core keeps pointers to this machine's board and RAM until the
        // next softcpu_init; make sure it doesn't call them meanwhile.
        // SAFETY: plain call; the core only runs inside softcpu_run.
        unsafe { softcpu_set_intr(0) };
    }
}

fn core_error() -> Error {
    // SAFETY: softcpu_error returns a NUL-terminated string owned by the core.
    let msg = unsafe { CStr::from_ptr(softcpu_error()) }.to_string_lossy().into_owned();
    // SAFETY: plain query.
    let rip = unsafe { softcpu_rip(0) };
    Error(format!("software CPU: {msg} (rip {rip:#x})"))
}

fn sleep(d: Duration) {
    #[cfg(windows)]
    {
        thread_local! {
            static TIMER: crate::timer::HrTimer = crate::timer::HrTimer::new();
        }
        TIMER.with(|t| t.sleep(d));
    }
    #[cfg(not(windows))]
    std::thread::sleep(d);
}
