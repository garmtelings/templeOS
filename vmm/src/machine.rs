//! The virtual machine: a WHPX partition with one or more vCPUs, guest
//! memory and the PC board, plus the run loops that service exits.
//!
//! Threads: [`Machine::run`] runs the bootstrap processor (vCPU 0) on the
//! caller's thread; each application processor (AP) has a thread of its
//! own. The board sits behind a mutex that every I/O or MMIO exit takes for
//! the one access it emulates.
//!
//! Interrupts:
//! - 8259 interrupts go to the BSP only (LINT0 in virtual-wire mode, as
//!   SeaBIOS leaves it). As in QEMU's WHPX backend with the in-hypervisor
//!   local APIC, they are injected as ExtINT pending events after an
//!   interrupt-window exit says the guest can take one.
//! - IPIs between vCPUs are delivered by the hypervisor's local APICs,
//!   except INIT and SIPI: those trap to us and are re-issued with
//!   `WHvRequestInterrupt`, as QEMU does. Before an INIT reaches an AP its
//!   thread is parked, so no stale exit handling can touch the fresh reset
//!   state; the SIPI that follows lets it run.
//! - A vCPU that executed HLT sleeps until the board (BSP) or its local
//!   APIC's IRR (any vCPU) shows an interrupt for it. With interrupts
//!   disabled at the HLT it stays halted until an INIT.
//!
//! Display: every 1/60 s of wall time the BSP loop renders the VGA into a
//! [`Frame`] and hands it to the display callback.

// Exit reasons are matched by the windows crate's constant names.
#![allow(non_upper_case_globals)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use devices::input::InputQueue;
use devices::pc::{Pc, PcConfig};
use devices::vga_render::{Frame, Renderer};
use devices::Nanos;
use windows::core::HRESULT;
use windows::Win32::Foundation::{E_FAIL, S_OK};
use windows::Win32::System::Hypervisor::*;

use crate::bitop;
use crate::cpuid::Cpuid;
use crate::events::{BoardNews, Events};
use crate::memory::{GuestMemory, MemRef, VGA_HOLE};
use crate::timer::HrTimer;
use crate::whpx::{self, Access, Partition};

/// Most vCPUs a machine can have (TempleOS handles up to 128; WHPX 64).
pub use crate::types::*;

/// How often a halted vCPU looks at its local APIC for a pending IPI.
const APIC_POLL: Duration = Duration::from_micros(250);


impl From<whpx::Error> for Error {
    fn from(e: whpx::Error) -> Self {
        Error(e.to_string())
    }
}

type Result<T> = std::result::Result<T, Error>;

/// The board and the VGA mapping that follows it (changed together, under
/// one lock, so the mapping is right before any vCPU resumes).
struct Board {
    pc: Pc,
    /// The VGA plane currently mapped at 0xA0000, if any.
    vga_mapped: Option<usize>,
}

/// Run state of one application processor's thread.
#[derive(Default)]
struct ApState {
    /// A SIPI has been delivered since the last INIT: the thread may run the vCPU.
    released: bool,
    /// Someone wants the thread to stop running the vCPU (an INIT is coming).
    want_park: bool,
    /// The thread is not running the vCPU and waits to be released.
    parked: bool,
}

struct ApControl {
    state: Mutex<ApState>,
    cv: Condvar,
}

/// What every vCPU thread shares.
struct Shared {
    part: Arc<Partition>,
    mem: GuestMemory,
    board: Mutex<Board>,
    cpuid: Cpuid,
    start: Instant,
    cpus: u32,
    vga_fast_path: AtomicBool,
    /// Tells the AP threads to exit.
    shutdown: AtomicBool,
    /// Index vp - 1.
    aps: Vec<ApControl>,
    /// An AP was started (for the ApStarted milestone).
    ap_started: AtomicBool,
    /// First fatal error on an AP thread, reported by the BSP loop.
    ap_error: Mutex<Option<String>>,
}

impl Shared {
    fn now(&self) -> Nanos {
        self.start.elapsed().as_nanos() as Nanos
    }

    fn board(&self) -> MutexGuard<'_, Board> {
        self.board.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Map or unmap a VGA plane at 0xA0000 to follow the VGA's state. Called
    /// after every emulated access (under the board lock), so the mapping is
    /// right before the vCPU that changed the VGA state runs again.
    fn update_vga_mapping(&self, board: &mut Board) -> Result<()> {
        let want = if self.vga_fast_path.load(Ordering::Relaxed) { board.pc.vga_fast_plane() } else { None };
        if want.map(|(p, _)| p) == board.vga_mapped {
            return Ok(());
        }
        let window = VGA_HOLE.start;
        let len = 0x10000;
        if board.vga_mapped.take().is_some() {
            self.part.unmap(window, len)?;
        }
        if let Some((plane, ptr)) = want {
            // SAFETY: the plane is a page-aligned 4 MiB block owned by the
            // VGA, which lives as long as the machine; the host only reads it
            // between runs or through raw pointers.
            unsafe { self.part.map(ptr, window, len, Access::ReadWrite)? };
            board.vga_mapped = Some(plane);
        }
        Ok(())
    }

    /// An INIT or SIPI the guest wrote to its ICR on `from` (the hypervisor
    /// trapped it): deliver it, parking or releasing AP threads around it.
    fn init_sipi(&self, from: u32, icr: u64) -> Result<()> {
        let mode = (icr >> 8) & 7;
        let logical = icr & (1 << 11) != 0;
        let level = icr & (1 << 14) != 0;
        let trigger_level = icr & (1 << 15) != 0;
        let vector = (icr & 0xff) as u32;
        let (kind, init) = match mode {
            5 => (WHvX64InterruptTypeInit, true),
            6 => (WHvX64InterruptTypeSipi, false),
            _ => return Err(Error(format!("unexpected INIT/SIPI trap, ICR {icr:#x}"))),
        };
        // INIT level de-assert does nothing on an xAPIC.
        if init && trigger_level && !level {
            return Ok(());
        }
        let targets: Vec<u32> = match (icr >> 18) & 3 {
            0 if logical => {
                // Logical destinations are resolved by the hypervisor; neither
                // SeaBIOS nor TempleOS sends INIT/SIPI this way.
                devices::unhandled("logical INIT/SIPI", icr, 8, None);
                self.part.request_interrupt(kind, true, level, (icr >> 56) as u32, vector)?;
                return Ok(());
            }
            0 => vec![(icr >> 56) as u32],
            1 => vec![from],
            2 => (0..self.cpus).collect(),
            _ => (0..self.cpus).filter(|&v| v != from).collect(),
        };
        for vp in targets.into_iter().filter(|&v| v < self.cpus) {
            if init {
                if vp != 0 && vp != from {
                    self.park(vp);
                }
                self.part.request_interrupt(kind, false, level, vp, vector)?;
            } else {
                self.part.request_interrupt(kind, false, level, vp, vector)?;
                if vp != 0 {
                    self.release(vp);
                }
            }
        }
        Ok(())
    }

    /// Stop AP `vp`'s thread from running its vCPU and wait until it has.
    fn park(&self, vp: u32) {
        let ap = &self.aps[vp as usize - 1];
        let mut st = ap.state.lock().unwrap();
        st.want_park = true;
        self.part.cancel(vp);
        while !st.parked && !self.shutdown.load(Ordering::Acquire) {
            st = ap.cv.wait_timeout(st, Duration::from_millis(5)).unwrap().0;
            self.part.cancel(vp);
        }
        st.released = false;
        st.want_park = false;
    }

    /// Let AP `vp`'s thread run its vCPU (a SIPI was delivered).
    fn release(&self, vp: u32) {
        let ap = &self.aps[vp as usize - 1];
        let mut st = ap.state.lock().unwrap();
        st.released = true;
        st.want_park = false;
        ap.cv.notify_all();
        self.ap_started.store(true, Ordering::Release);
    }
}

pub struct Machine {
    shared: Arc<Shared>,
    ap_threads: Vec<JoinHandle<()>>,
    emulator: Emulator,
    kicker: Kicker,
    timer: HrTimer,
    /// The last exit was an interrupt window: an ExtINT may be injected now.
    ready_for_pic: bool,
    /// An interrupt-window notification is requested and not yet delivered.
    window_registered: bool,
    /// The BSP executed HLT and waits for an interrupt; the flag is RFLAGS.IF
    /// at the HLT.
    halted: Option<bool>,
    events: Events,
    renderer: Renderer,
    frame: Frame,
    next_frame: Nanos,
    display: Option<DisplayFn>,
    stop_flag: Option<Arc<AtomicBool>>,
    input: Option<Arc<InputQueue>>,
}

const BSP: u32 = 0;

impl Machine {
    pub fn new(cfg: Config) -> Result<Self> {
        whpx::check_available().map_err(Error)?;
        let cpus = cfg.cpus.clamp(1, MAX_CPUS);
        let cpuid = Cpuid::qemu64_smp(cpus);
        let part = Arc::new(Partition::new(cpus, &cpuid.leaves())?);
        let mut mem = GuestMemory::new(cfg.ram_size, cfg.bios);
        mem.map_into(&part)?;
        for vp in 0..cpus {
            part.create_vp(vp)?;
        }
        // APs start in wait-for-SIPI, like real ones after reset.
        for vp in 1..cpus {
            let startup_suspend = WHV_REGISTER_VALUE { Reg64: 1 };
            let _ = part.set_regs(vp, &[WHvRegisterInternalActivityState], &[startup_suspend]);
        }
        let pc = Pc::new(PcConfig {
            ram_size: cfg.ram_size,
            vgabios: cfg.vgabios,
            cdrom: cfg.cdrom,
            hdd: cfg.hdd,
            rtc_base: cfg.rtc_base,
            cpus: cpus as u8,
        });
        let start = Instant::now();
        let shared = Arc::new(Shared {
            part: part.clone(),
            mem,
            board: Mutex::new(Board { pc, vga_mapped: None }),
            cpuid,
            start,
            cpus,
            vga_fast_path: AtomicBool::new(cfg.vga_fast_path),
            shutdown: AtomicBool::new(false),
            aps: (1..cpus)
                .map(|_| ApControl { state: Mutex::new(ApState { parked: true, ..Default::default() }), cv: Condvar::new() })
                .collect(),
            ap_started: AtomicBool::new(false),
            ap_error: Mutex::new(None),
        });
        let m = Machine {
            ap_threads: (1..cpus)
                .map(|vp| {
                    let s = shared.clone();
                    std::thread::Builder::new()
                        .name(format!("vcpu{vp}"))
                        .spawn(move || ap_thread(&s, vp))
                        .expect("spawn vCPU thread")
                })
                .collect(),
            shared,
            emulator: Emulator::new()?,
            kicker: Kicker::spawn(part, start),
            timer: HrTimer::new(),
            ready_for_pic: false,
            window_registered: false,
            halted: None,
            events: Events::new(start),
            renderer: Renderer::new(),
            frame: Frame::default(),
            next_frame: 0,
            display: None,
            stop_flag: None,
            input: None,
        };
        m.reset_bsp()?;
        Ok(m)
    }

    /// Also copy the raw debug console (port 0x402) output to `w`.
    pub fn set_debugcon_sink(&mut self, w: Box<dyn std::io::Write>, echo: bool) {
        self.events.set_debugcon_sink(w, echo);
    }

    /// Run `f` on the board (locked).
    pub fn with_pc<R>(&self, f: impl FnOnce(&mut Pc) -> R) -> R {
        f(&mut self.shared.board().pc)
    }

    /// Log every port and MMIO access (see [`Pc::set_trace`]). Tracing sees
    /// only emulated accesses, so it turns the VGA fast path off.
    pub fn set_trace(&mut self, w: Box<dyn std::io::Write + Send>) {
        self.shared.board().pc.set_trace(w);
        self.shared.vga_fast_path.store(false, Ordering::Relaxed);
    }

    /// Call `f` with a freshly rendered frame 60 times a second while running.
    pub fn set_display(&mut self, f: DisplayFn) {
        self.display = Some(f);
    }

    /// [`Machine::run`] returns [`Stop::Quit`] soon after `flag` becomes true.
    pub fn set_stop_flag(&mut self, flag: Arc<AtomicBool>) {
        self.stop_flag = Some(flag);
    }

    /// Keyboard and mouse events from `queue` are delivered to the guest
    /// between vCPU runs.
    pub fn set_input(&mut self, queue: Arc<InputQueue>) {
        self.input = Some(queue);
    }

    /// Call `f` whenever the PC speaker starts, stops or changes pitch.
    pub fn set_speaker(&mut self, f: SpeakerFn) {
        self.events.speaker = Some(f);
    }

    /// Render the display as it is now.
    pub fn render(&mut self) -> &Frame {
        let now = self.shared.now();
        self.renderer.render(self.shared.board().pc.vga(), now, &mut self.frame);
        &self.frame
    }

    pub fn memory(&self) -> &GuestMemory {
        &self.shared.mem
    }

    pub fn reached(&self) -> &[Milestone] {
        &self.events.reached
    }

    pub fn cpus(&self) -> u32 {
        self.shared.cpus
    }

    /// Put the BSP in the x86 power-on state QEMU uses: CS:IP = F000:FFF0
    /// with CS base 0xFFFF0000, EDX = the CPUID(1) signature.
    fn reset_bsp(&self) -> Result<()> {
        let cs = WHV_REGISTER_VALUE {
            Segment: WHV_X64_SEGMENT_REGISTER {
                Base: 0xFFFF_0000,
                Limit: 0xFFFF,
                Selector: 0xF000,
                Anonymous: WHV_X64_SEGMENT_REGISTER_0 { Attributes: 0x9B },
            },
        };
        let names = [WHvX64RegisterCs, WHvX64RegisterRip, WHvX64RegisterRflags, WHvX64RegisterRdx];
        let values = [
            cs,
            WHV_REGISTER_VALUE { Reg64: 0xFFF0 },
            WHV_REGISTER_VALUE { Reg64: 0x2 },
            WHV_REGISTER_VALUE { Reg64: self.shared.cpuid.query(1, 0, 0).eax as u64 },
        ];
        self.shared.part.set_regs(BSP, &names, &values)?;
        Ok(())
    }

    /// Run until `until` is reached, `limit` of wall time passes, or the
    /// guest asks for a reset. The BSP runs on this thread; the APs keep
    /// running on theirs until the machine is dropped.
    pub fn run(&mut self, until: Option<Milestone>, limit: Option<Duration>) -> Result<Stop> {
        let deadline = limit.map(|l| Instant::now() + l);
        let debug = std::env::var_os("TEMPLEOS_DEBUG").is_some();
        let log_exits = std::env::var("TEMPLEOS_DEBUG").is_ok_and(|v| v == "exits");
        let mut counts = std::collections::BTreeMap::<i32, u64>::new();
        let mut last_report = Instant::now();
        let shared = self.shared.clone();
        loop {
            if debug && last_report.elapsed() >= Duration::from_secs(1) {
                last_report = Instant::now();
                let rip = shared.part.get_reg(BSP, WHvX64RegisterRip).map(|v| unsafe { v.Reg64 }).unwrap_or(0);
                eprintln!("[debug] exits {counts:?} rip={rip:#x} pic_int={}", shared.board().pc.has_interrupt());
                if std::env::var("TEMPLEOS_DEBUG").is_ok_and(|v| v == "regs") {
                    let ram = shared.mem.ram();
                    let at = (rip as usize).saturating_sub(32).min(ram.len().saturating_sub(96));
                    eprintln!(
                        "[debug] now={} halted={:?} window={} {}\n{}\n[debug] code at {at:#x}: {:02x?}",
                        shared.now(),
                        self.halted,
                        self.window_registered,
                        shared.board().pc.irq_debug(),
                        dump_regs(&shared.part, BSP),
                        &ram[at..at + 96]
                    );
                }
            }
            if let Some(e) = shared.ap_error.lock().unwrap().take() {
                return Err(Error(e));
            }
            let now = shared.now();
            {
                let mut board = shared.board();
                board.pc.poll(now);
                if let Some(q) = &self.input {
                    for ev in q.take() {
                        board.pc.input(ev);
                    }
                }
            }
            if let Some(stop) = self.check_events(until) {
                return Ok(stop);
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(Stop::TimeLimit);
            }
            if self.stop_flag.as_ref().is_some_and(|f| f.load(Ordering::Relaxed)) {
                return Ok(Stop::Quit);
            }
            if self.display.is_some() && now >= self.next_frame {
                self.next_frame = now + FRAME_NS;
                self.renderer.render(shared.board().pc.vga(), now, &mut self.frame);
                if let Some(show) = &mut self.display {
                    show(&self.frame);
                }
            }
            let wake = self.next_wake(now);

            if let Some(interrupts_enabled) = self.halted {
                let pending = interrupts_enabled
                    && (shared.board().pc.has_interrupt() || (shared.cpus > 1 && shared.part.apic_irr_pending(BSP)));
                if !pending {
                    // HLT with nothing to deliver: sleep until the next timer
                    // event or frame (or APIC poll), then look again.
                    let mut nap = Duration::from_nanos(wake.saturating_sub(now));
                    if shared.cpus > 1 {
                        nap = nap.min(APIC_POLL);
                    }
                    self.timer.sleep(nap);
                    continue;
                }
                self.halted = None;
            }

            {
                let mut board = shared.board();
                shared.update_vga_mapping(&mut board)?;
            }
            self.inject_interrupts()?;
            self.kicker.arm(wake);
            let exit = shared.part.run(BSP);
            self.kicker.disarm();
            let exit = exit?;
            *counts.entry(exit.ExitReason.0).or_default() += 1;
            if log_exits {
                // SAFETY: diagnostic read of the port number; garbage for other exits is fine.
                let port = unsafe { exit.Anonymous.IoPortAccess.PortNumber };
                eprintln!("[exit] {:#x} rip={:#x} port={port:#x}", exit.ExitReason.0, exit.VpContext.Rip);
            }

            // SAFETY: WHV_X64_VP_EXECUTION_STATE is a bitfield over a u16.
            let state = unsafe { exit.VpContext.ExecutionState.AsUINT16 };
            if state & (1 << 4) != 0 {
                self.events.reach(Milestone::LongMode);
            }

            match exit.ExitReason {
                WHvRunVpExitReasonX64InterruptWindow => {
                    self.ready_for_pic = true;
                    self.window_registered = false;
                }
                _ => match handle_exit(&shared, BSP, &self.emulator, &exit)? {
                    Exit::Continue => {}
                    Exit::Halt(interrupts_enabled) => self.halted = Some(interrupts_enabled),
                },
            }
        }
    }

    fn check_events(&mut self, until: Option<Milestone>) -> Option<Stop> {
        let news = BoardNews::take(&mut self.shared.board().pc);
        let ap_started = self.shared.ap_started.load(Ordering::Acquire);
        self.events.handle(news, ap_started, until)
    }

    /// Before entering the guest: hand it a pending 8259 interrupt if the
    /// last exit opened an interrupt window, otherwise ask for a window.
    fn inject_interrupts(&mut self) -> Result<()> {
        let ready = std::mem::take(&mut self.ready_for_pic);
        let mut board = self.shared.board();
        if !board.pc.has_interrupt() {
            return Ok(());
        }
        let part = &self.shared.part;
        if ready {
            let vector = board.pc.acknowledge_interrupt();
            // WHV_X64_PENDING_EXT_INT_EVENT: EventPending (bit 0), EventType (bits 1-3), Vector (bits 8-15).
            let event = 1u64 | (WHvX64PendingEventExtInt.0 as u64) << 1 | (vector as u64) << 8;
            let value = WHV_REGISTER_VALUE { Reg128: WHV_UINT128 { Anonymous: WHV_UINT128_0 { Low64: event, High64: 0 } } };
            part.set_regs(BSP, &[WHvRegisterPendingEvent], &[value])?;
            // With its local APIC emulated, the hypervisor handles HLT itself
            // (no Halt exit), and a pending ExtINT does not wake a vCPU it
            // has suspended in HLT: take it out of that state, as the
            // interrupt would on a real CPU.
            // WHV_INTERNAL_ACTIVITY_REGISTER: HaltSuspend is bit 1.
            const HALT_SUSPEND: u64 = 1 << 1;
            // SAFETY: the activity state is a 64-bit register.
            if let Ok(state) = part.get_reg(BSP, WHvRegisterInternalActivityState).map(|v| unsafe { v.Reg64 }) {
                if state & HALT_SUSPEND != 0 {
                    let awake = WHV_REGISTER_VALUE { Reg64: state & !HALT_SUSPEND };
                    part.set_regs(BSP, &[WHvRegisterInternalActivityState], &[awake])?;
                }
            }
        } else if !self.window_registered {
            // WHV_X64_DELIVERABILITY_NOTIFICATIONS_REGISTER: InterruptNotification is bit 1.
            let value = WHV_REGISTER_VALUE { Reg64: 1 << 1 };
            part.set_regs(BSP, &[WHvX64RegisterDeliverabilityNotifications], &[value])?;
            self.window_registered = true;
        }
        Ok(())
    }

    /// When the BSP loop must next look at the machine: the next device
    /// timer, the next frame, and at most 10 ms from now.
    fn next_wake(&self, now: Nanos) -> Nanos {
        let mut wake = now + 10_000_000;
        if let Some(t) = self.shared.board().pc.next_deadline() {
            wake = wake.min(t);
        }
        if self.display.is_some() {
            wake = wake.min(self.next_frame);
        }
        wake
    }

    /// A register dump of the BSP for error reports.
    pub fn dump_regs(&self) -> String {
        dump_regs(&self.shared.part, BSP)
    }
}

impl Drop for Machine {
    /// Stop and join the AP threads before the partition and memory go.
    fn drop(&mut self) {
        let s = &self.shared;
        s.shutdown.store(true, Ordering::Release);
        for (i, ap) in s.aps.iter().enumerate() {
            let _st = ap.state.lock().unwrap();
            ap.cv.notify_all();
            s.part.cancel(i as u32 + 1);
        }
        for t in self.ap_threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// What a vCPU loop does after an exit.
enum Exit {
    Continue,
    /// HLT; the flag is RFLAGS.IF at the HLT.
    Halt(bool),
}

/// Exit handling shared by all vCPUs (the BSP handles interrupt windows
/// itself).
fn handle_exit(shared: &Shared, vp: u32, emulator: &Emulator, exit: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<Exit> {
    match exit.ExitReason {
        WHvRunVpExitReasonMemoryAccess => emulate(shared, vp, emulator, exit, false)?,
        WHvRunVpExitReasonX64IoPortAccess => emulate(shared, vp, emulator, exit, true)?,
        WHvRunVpExitReasonX64Cpuid => cpuid_exit(shared, vp, exit)?,
        WHvRunVpExitReasonX64Halt => return Ok(Exit::Halt(exit.VpContext.Rflags & 0x200 != 0)),
        WHvRunVpExitReasonX64ApicInitSipiTrap => {
            // SAFETY: the exit reason says ApicInitSipi is the active member.
            let icr = unsafe { exit.Anonymous.ApicInitSipi.ApicIcr };
            shared.init_sipi(vp, icr)?;
        }
        WHvRunVpExitReasonCanceled | WHvRunVpExitReasonX64ApicEoi => {}
        other => {
            return Err(Error(format!(
                "vCPU {vp} stopped: exit reason {:#x}\n{}",
                other.0,
                dump_regs(&shared.part, vp)
            )))
        }
    }
    Ok(Exit::Continue)
}

/// An application processor's thread: wait to be released by a SIPI, then
/// run the vCPU; park again when an INIT is on its way.
fn ap_thread(shared: &Shared, vp: u32) {
    let result = (|| -> Result<()> {
        let emulator = Emulator::new()?;
        let timer = HrTimer::new();
        let ap = &shared.aps[vp as usize - 1];
        let mut halted: Option<bool> = None;
        loop {
            {
                let mut st = ap.state.lock().unwrap();
                if st.want_park || !st.released {
                    st.parked = true;
                    ap.cv.notify_all();
                    while (st.want_park || !st.released) && !shared.shutdown.load(Ordering::Acquire) {
                        st = ap.cv.wait(st).unwrap();
                    }
                    st.parked = false;
                    // Whatever the vCPU was doing, INIT/SIPI replaced it.
                    halted = None;
                }
            }
            if shared.shutdown.load(Ordering::Acquire) {
                return Ok(());
            }
            if let Some(interrupts_enabled) = halted {
                if !(interrupts_enabled && shared.part.apic_irr_pending(vp)) {
                    timer.sleep(APIC_POLL);
                    continue;
                }
                halted = None;
            }
            let exit = shared.part.run(vp)?;
            if let Exit::Halt(interrupts_enabled) = handle_exit(shared, vp, &emulator, &exit)? {
                halted = Some(interrupts_enabled);
            }
        }
    })();
    if let Err(e) = result {
        shared.ap_error.lock().unwrap().get_or_insert(format!("vCPU {vp}: {e}"));
        shared.part.cancel(BSP);
    }
}

fn cpuid_exit(shared: &Shared, vp: u32, exit: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<()> {
    // SAFETY: the exit reason says this union member is the active one.
    let c = unsafe { exit.Anonymous.CpuidAccess };
    // APIC IDs are the VP indexes (WHPX's, and QEMU's for these topologies).
    let r = shared.cpuid.query(c.Rax as u32, c.Rcx as u32, vp as u8);
    let len = (exit.VpContext._bitfield & 0xF) as u64;
    let names = [WHvX64RegisterRax, WHvX64RegisterRbx, WHvX64RegisterRcx, WHvX64RegisterRdx, WHvX64RegisterRip];
    let values = [r.eax as u64, r.ebx as u64, r.ecx as u64, r.edx as u64, exit.VpContext.Rip + len]
        .map(|v| WHV_REGISTER_VALUE { Reg64: v });
    shared.part.set_regs(vp, &names, &values)?;
    Ok(())
}

/// Emulate the I/O or MMIO instruction behind `exit` with the WHPX
/// instruction emulator, then bring the VGA mapping up to date and, if an
/// AP's access raised a board interrupt, kick the BSP to deliver it.
fn emulate(shared: &Shared, vp: u32, emulator: &Emulator, exit: &WHV_RUN_VP_EXIT_CONTEXT, io: bool) -> Result<()> {
    let mut ctx = EmuContext { shared, vp };
    // SAFETY: the exit reason says which union member is active; ctx outlives
    // the call.
    let status = unsafe {
        if io {
            WHvEmulatorTryIoEmulation(
                emulator.handle,
                &mut ctx as *mut EmuContext as _,
                &exit.VpContext,
                &exit.Anonymous.IoPortAccess,
            )
        } else {
            WHvEmulatorTryMmioEmulation(
                emulator.handle,
                &mut ctx as *mut EmuContext as _,
                &exit.VpContext,
                &exit.Anonymous.MemoryAccess,
            )
        }
    };
    let what = if io { "I/O" } else { "MMIO" };
    let status = status.map_err(|e| Error(format!("{what} emulation call failed: {e}")))?;
    // SAFETY: bitfield over a u32; bit 0 is EmulationSuccessful.
    let bits = unsafe { status.AsUINT32 };
    // Bit 1 is InternalEmulationFailure: an instruction it doesn't implement.
    // SAFETY: a non-I/O exit here is a memory access.
    let emulated = bits & 1 != 0 || (!io && bits & 2 != 0 && emulate_bitop(shared, vp, unsafe { &exit.Anonymous.MemoryAccess })?);
    if !emulated {
        // SAFETY: reading the exit union for diagnostics only.
        let detail = unsafe {
            if io {
                format!("port {:#x}", exit.Anonymous.IoPortAccess.PortNumber)
            } else {
                let m = &exit.Anonymous.MemoryAccess;
                let n = (m.InstructionByteCount as usize).min(m.InstructionBytes.len());
                format!("gpa {:#x}, instruction {:02x?}", m.Gpa, &m.InstructionBytes[..n])
            }
        };
        return Err(Error(format!(
            "vCPU {vp}: {what} emulation failed at {detail} (status {bits:#x})\n{}",
            dump_regs(&shared.part, vp)
        )));
    }
    let mut board = shared.board();
    shared.update_vga_mapping(&mut board)?;
    if vp != BSP && board.pc.has_interrupt() {
        shared.part.cancel(BSP);
    }
    Ok(())
}

/// WHPX's emulator doesn't implement BT/BTS/BTR/BTC: run one on device
/// memory here (see [`bitop`]). False if the instruction isn't one.
fn emulate_bitop(shared: &Shared, vp: u32, m: &WHV_MEMORY_ACCESS_CONTEXT) -> Result<bool> {
    use devices::GuestMemory as _;
    let names = [WHvX64RegisterRip, WHvX64RegisterRflags, WHvX64RegisterCs, WHvX64RegisterCr0, WHvX64RegisterEfer];
    let mut regs = [WHV_REGISTER_VALUE::default(); 5];
    shared.part.get_regs(vp, &names, &mut regs)?;
    // SAFETY: CS is a segment register, the others are 64-bit.
    let (rip, rflags, cs_attr, cr0, efer) =
        unsafe { (regs[0].Reg64, regs[1].Reg64, regs[2].Segment.Anonymous.Attributes, regs[3].Reg64, regs[4].Reg64) };
    // Code size: CR0.PE, EFER.LMA, CS.L, CS.D.
    let mode = match (cr0 & 1 != 0, efer & 1 << 10 != 0, cs_attr & 1 << 13 != 0, cs_attr & 1 << 14 != 0) {
        (false, ..) => 16,
        (true, true, true, _) => 64,
        (true, _, _, true) => 32,
        _ => 16,
    };
    let n = (m.InstructionByteCount as usize).min(m.InstructionBytes.len());
    let Some(insn) = bitop::decode(&m.InstructionBytes[..n], mode) else {
        return Ok(false);
    };
    let number = match insn.bit {
        bitop::BitSource::Imm(b) => b as u64,
        // SAFETY: general-purpose registers are 64-bit; 0-15 are RAX-R15.
        bitop::BitSource::Reg(r) => unsafe { shared.part.get_reg(vp, WHV_REGISTER_NAME(r as i32))?.Reg64 },
    };
    // The CPU has already computed the operand's address: the exit's GPA.
    let (gpa, size) = (m.Gpa, insn.size as usize);
    let now = shared.now();
    // Read-modify-write under the board lock, so LOCK's atomicity holds
    // against the other vCPUs.
    let mut board = shared.board();
    let mut buf = [0u8; 8];
    let value = if shared.mem.read(gpa, &mut buf[..size]) {
        u64::from_le_bytes(buf)
    } else {
        board.pc.mmio_read(gpa, size as u8, now)
    };
    let (new, flags) = bitop::execute(insn.op, insn.size, value, insn.bit_index(number), rflags);
    if insn.op != bitop::Op::Test && !shared.mem.write_shared(gpa, &new.to_le_bytes()[..size]) {
        board.pc.mmio_write(gpa, size as u8, new, now);
    }
    drop(board);
    let values = [rip + insn.len as u64, flags].map(|v| WHV_REGISTER_VALUE { Reg64: v });
    shared.part.set_regs(vp, &[WHvX64RegisterRip, WHvX64RegisterRflags], &values)?;
    Ok(true)
}

/// A register dump for error reports.
fn dump_regs(part: &Partition, vp: u32) -> String {
    let names = [
        (WHvX64RegisterRip, "rip"),
        (WHvX64RegisterRflags, "rflags"),
        (WHvX64RegisterRax, "rax"),
        (WHvX64RegisterRbx, "rbx"),
        (WHvX64RegisterRcx, "rcx"),
        (WHvX64RegisterRdx, "rdx"),
        (WHvX64RegisterRsi, "rsi"),
        (WHvX64RegisterRdi, "rdi"),
        (WHvX64RegisterRsp, "rsp"),
        (WHvX64RegisterRbp, "rbp"),
        (WHvX64RegisterCr0, "cr0"),
        (WHvX64RegisterCr2, "cr2"),
        (WHvX64RegisterCr3, "cr3"),
        (WHvX64RegisterCr4, "cr4"),
        (WHvX64RegisterEfer, "efer"),
    ];
    let mut out = String::new();
    for (i, (name, label)) in names.iter().enumerate() {
        match part.get_reg(vp, *name) {
            // SAFETY: all of these are 64-bit registers.
            Ok(v) => out += &format!("{label:>6}={:016x}", unsafe { v.Reg64 }),
            Err(_) => out += &format!("{label:>6}=?"),
        }
        out.push(if i % 4 == 3 { '\n' } else { ' ' });
    }
    if let Ok(cs) = part.get_reg(vp, WHvX64RegisterCs) {
        // SAFETY: CS is a segment register.
        let s = unsafe { cs.Segment };
        out += &format!("\n    cs={:04x} base={:x} attr={:04x}", s.Selector, s.Base, unsafe { s.Anonymous.Attributes });
    }
    out
}

/// State the instruction emulator's callbacks need during one emulation.
struct EmuContext<'a> {
    shared: &'a Shared,
    vp: u32,
}

/// The WHPX instruction emulator (decodes the exiting I/O or MMIO
/// instruction and calls back into us for the device access). One per vCPU
/// thread.
struct Emulator {
    handle: *mut core::ffi::c_void,
}

impl Emulator {
    fn new() -> Result<Self> {
        let callbacks = WHV_EMULATOR_CALLBACKS {
            Size: std::mem::size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
            Reserved: 0,
            WHvEmulatorIoPortCallback: Some(emu_io),
            WHvEmulatorMemoryCallback: Some(emu_memory),
            WHvEmulatorGetVirtualProcessorRegisters: Some(emu_get_regs),
            WHvEmulatorSetVirtualProcessorRegisters: Some(emu_set_regs),
            WHvEmulatorTranslateGvaPage: Some(emu_translate),
        };
        let mut handle = std::ptr::null_mut();
        // SAFETY: callbacks is a valid table; handle is an out-pointer.
        unsafe { WHvEmulatorCreateEmulator(&callbacks, &mut handle) }
            .map_err(|e| Error(format!("WHvEmulatorCreateEmulator: {e}")))?;
        Ok(Emulator { handle })
    }
}

impl Drop for Emulator {
    fn drop(&mut self) {
        // SAFETY: handle came from WHvEmulatorCreateEmulator.
        let _ = unsafe { WHvEmulatorDestroyEmulator(self.handle) };
    }
}

// The emulator callbacks. `ctx` is always the &mut EmuContext passed to
// WHvEmulatorTry*Emulation, which is alive for the duration of the call.

unsafe extern "system" fn emu_io(
    ctx: *const core::ffi::c_void,
    io: *mut WHV_EMULATOR_IO_ACCESS_INFO,
) -> HRESULT {
    let ctx = &*(ctx as *const EmuContext);
    let io = &mut *io;
    let size = io.AccessSize as u8;
    let now = ctx.shared.now();
    let mut board = ctx.shared.board();
    if io.Direction == 0 {
        io.Data = board.pc.io_read(io.Port, size, now);
    } else {
        board.pc.io_write(io.Port, size, io.Data, now, &mut MemRef(&ctx.shared.mem));
    }
    S_OK
}

unsafe extern "system" fn emu_memory(
    ctx: *const core::ffi::c_void,
    ma: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO,
) -> HRESULT {
    use devices::GuestMemory as _;
    let ctx = &*(ctx as *const EmuContext);
    let ma = &mut *ma;
    let size = (ma.AccessSize as usize).min(8);
    let data = &mut ma.Data[..size];
    let mem = &ctx.shared.mem;
    if ma.Direction == 0 {
        if !mem.read(ma.GpaAddress, data) {
            let v = ctx.shared.board().pc.mmio_read(ma.GpaAddress, size as u8, ctx.shared.now());
            data.copy_from_slice(&v.to_le_bytes()[..size]);
        }
    } else if !mem.write_shared(ma.GpaAddress, data) {
        let mut buf = [0u8; 8];
        buf[..size].copy_from_slice(data);
        ctx.shared.board().pc.mmio_write(ma.GpaAddress, size as u8, u64::from_le_bytes(buf), ctx.shared.now());
    }
    S_OK
}

unsafe extern "system" fn emu_get_regs(
    ctx: *const core::ffi::c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *mut WHV_REGISTER_VALUE,
) -> HRESULT {
    let ctx = &*(ctx as *const EmuContext);
    let names = std::slice::from_raw_parts(names, count as usize);
    let values = std::slice::from_raw_parts_mut(values, count as usize);
    match ctx.shared.part.get_regs(ctx.vp, names, values) {
        Ok(()) => S_OK,
        Err(_) => E_FAIL,
    }
}

unsafe extern "system" fn emu_set_regs(
    ctx: *const core::ffi::c_void,
    names: *const WHV_REGISTER_NAME,
    count: u32,
    values: *const WHV_REGISTER_VALUE,
) -> HRESULT {
    let ctx = &*(ctx as *const EmuContext);
    let names = std::slice::from_raw_parts(names, count as usize);
    let values = std::slice::from_raw_parts(values, count as usize);
    match ctx.shared.part.set_regs(ctx.vp, names, values) {
        Ok(()) => S_OK,
        Err(_) => E_FAIL,
    }
}

unsafe extern "system" fn emu_translate(
    ctx: *const core::ffi::c_void,
    gva: u64,
    flags: WHV_TRANSLATE_GVA_FLAGS,
    result: *mut WHV_TRANSLATE_GVA_RESULT_CODE,
    gpa: *mut u64,
) -> HRESULT {
    let ctx = &*(ctx as *const EmuContext);
    match ctx.shared.part.translate_gva(ctx.vp, gva, flags) {
        Ok((code, addr)) => {
            *result = code;
            *gpa = addr;
            S_OK
        }
        Err(_) => E_FAIL,
    }
}

/// Kicks the BSP out of the guest when the next device deadline arrives
/// (the vCPU thread is blocked inside WHvRunVirtualProcessor until then).
struct Kicker {
    /// Deadline in ns since machine start; u64::MAX when disarmed.
    deadline: Arc<AtomicU64>,
    /// Ends the thread (which holds a reference to the partition).
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Kicker {
    fn spawn(part: Arc<Partition>, start: Instant) -> Self {
        let deadline = Arc::new(AtomicU64::new(u64::MAX));
        let stop = Arc::new(AtomicBool::new(false));
        let d = deadline.clone();
        let quit = stop.clone();
        let thread = std::thread::Builder::new()
            .name("vcpu-kicker".into())
            .spawn(move || {
                let timer = HrTimer::new();
                while !quit.load(Ordering::Acquire) {
                    let now = start.elapsed().as_nanos() as u64;
                    let due = d.load(Ordering::Acquire);
                    if due != u64::MAX && now >= due {
                        if d.compare_exchange(due, u64::MAX, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                            part.cancel(BSP);
                        }
                        continue;
                    }
                    // Re-check at least every millisecond so a newly armed,
                    // earlier deadline is noticed.
                    timer.sleep(Duration::from_nanos(due.saturating_sub(now).min(1_000_000)));
                }
            })
            .expect("spawn kicker thread");
        Kicker { deadline, stop, thread: Some(thread) }
    }

    fn arm(&self, at: Nanos) {
        self.deadline.store(at, Ordering::Release);
    }

    fn disarm(&self) {
        self.deadline.store(u64::MAX, Ordering::Release);
    }
}

impl Drop for Kicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
