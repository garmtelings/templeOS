//! The virtual machine: a WHPX partition with one vCPU, guest memory and the
//! PC board, plus the run loop that services exits.
//!
//! Interrupt delivery follows QEMU's WHPX backend with the in-hypervisor
//! local APIC: 8259 interrupts are injected as ExtINT pending events, only
//! after an interrupt-window exit says the guest can take one.
//!
//! Display: every 1/60 s of wall time the run loop renders the VGA into a
//! [`Frame`] and hands it to the display callback. Rendering happens on the
//! vCPU thread between runs, so it always sees a consistent VGA state.

// Exit reasons are matched by the windows crate's constant names.
#![allow(non_upper_case_globals)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use devices::pc::{Pc, PcConfig};
use devices::vga_render::{Frame, Renderer};
use devices::Nanos;
use windows::core::HRESULT;
use windows::Win32::Foundation::{E_FAIL, S_OK};
use windows::Win32::System::Hypervisor::*;

use crate::cpuid::Cpuid;
use crate::memory::GuestMemory;
use crate::timer::HrTimer;
use crate::memory::VGA_HOLE;
use crate::whpx::{self, Access, Partition};

pub struct Config {
    pub ram_size: u64,
    pub bios: &'static [u8],
    pub vgabios: &'static [u8],
    pub cdrom: Option<&'static [u8]>,
    /// Guest wall-clock time at power-on, in Unix seconds.
    pub rtc_base: i64,
    /// Map a VGA plane straight into the guest while that is exact
    /// ([`devices::vga::Vga::fast_plane`]). Off means every access to the
    /// VGA window is emulated.
    pub vga_fast_path: bool,
}

/// Receives each rendered frame (see [`Machine::set_display`]).
pub type DisplayFn = Box<dyn FnMut(&Frame)>;

/// Wall time between rendered frames.
const FRAME_NS: Nanos = 1_000_000_000 / 60;

/// Points in the boot the run loop recognizes and reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Milestone {
    /// SeaBIOS printed its version banner on the debug console.
    BiosBanner,
    /// The vCPU is in long mode (EFER.LMA) for the first time.
    LongMode,
    /// The kernel programmed PIT channel 0 for its 1 kHz tick (TimersInit).
    KernelTimers,
}

impl Milestone {
    pub fn describe(self) -> &'static str {
        match self {
            Milestone::BiosBanner => "SeaBIOS banner on the debug console",
            Milestone::LongMode => "guest entered long mode",
            Milestone::KernelTimers => "kernel programmed the PIT (TimersInit)",
        }
    }
}

/// Why [`Machine::run`] returned.
#[derive(Debug)]
pub enum Stop {
    Milestone(Milestone),
    TimeLimit,
    ResetRequested,
    /// The stop flag was set (e.g. the window was closed).
    Quit,
}

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<whpx::Error> for Error {
    fn from(e: whpx::Error) -> Self {
        Error(e.to_string())
    }
}

type Result<T> = std::result::Result<T, Error>;

pub struct Machine {
    part: Arc<Partition>,
    mem: GuestMemory,
    pc: Pc,
    cpuid: Cpuid,
    emulator: Emulator,
    start: Instant,
    kicker: Kicker,
    timer: HrTimer,
    /// The last exit was an interrupt window: an ExtINT may be injected now.
    ready_for_pic: bool,
    /// An interrupt-window notification is requested and not yet delivered.
    window_registered: bool,
    /// The vCPU executed HLT and waits for an interrupt. The flag is RFLAGS.IF
    /// at the HLT: with interrupts disabled only an NMI (which the board never
    /// raises) could wake it, so it stays halted.
    halted: Option<bool>,
    reached: Vec<Milestone>,
    /// Debug console output not yet split into lines.
    debugcon_line: Vec<u8>,
    debugcon_sink: Option<Box<dyn std::io::Write>>,
    echo_debugcon: bool,
    vga_fast_path: bool,
    /// The VGA plane currently mapped at 0xA0000, if any.
    vga_mapped: Option<usize>,
    renderer: Renderer,
    frame: Frame,
    next_frame: Nanos,
    display: Option<DisplayFn>,
    stop_flag: Option<Arc<AtomicBool>>,
}

const VP: u32 = 0;

impl Machine {
    pub fn new(cfg: Config) -> Result<Self> {
        if !whpx::hypervisor_present() {
            return Err(Error(
                "the Windows Hypervisor Platform is not available. Enable it with (admin PowerShell):\n  \
                 Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform\nthen reboot."
                    .into(),
            ));
        }
        let cpuid = Cpuid::qemu64();
        let part = Arc::new(Partition::new(1, &cpuid.leaves())?);
        let mut mem = GuestMemory::new(cfg.ram_size, cfg.bios);
        mem.map_into(&part)?;
        part.create_vp(VP)?;
        let pc = Pc::new(PcConfig {
            ram_size: cfg.ram_size,
            vgabios: cfg.vgabios,
            cdrom: cfg.cdrom,
            rtc_base: cfg.rtc_base,
        });
        let start = Instant::now();
        let mut m = Machine {
            emulator: Emulator::new()?,
            kicker: Kicker::spawn(part.clone(), start),
            part,
            mem,
            pc,
            cpuid,
            start,
            timer: HrTimer::new(),
            ready_for_pic: false,
            window_registered: false,
            halted: None,
            reached: Vec::new(),
            debugcon_line: Vec::new(),
            debugcon_sink: None,
            echo_debugcon: true,
            vga_fast_path: cfg.vga_fast_path,
            vga_mapped: None,
            renderer: Renderer::new(),
            frame: Frame::default(),
            next_frame: 0,
            display: None,
            stop_flag: None,
        };
        m.reset_vcpu()?;
        Ok(m)
    }

    /// Also copy the raw debug console (port 0x402) output to `w`.
    pub fn set_debugcon_sink(&mut self, w: Box<dyn std::io::Write>, echo: bool) {
        self.debugcon_sink = Some(w);
        self.echo_debugcon = echo;
    }

    pub fn pc_mut(&mut self) -> &mut Pc {
        &mut self.pc
    }

    /// Log every port and MMIO access (see [`Pc::set_trace`]). Tracing sees
    /// only emulated accesses, so it turns the VGA fast path off.
    pub fn set_trace(&mut self, w: Box<dyn std::io::Write + Send>) {
        self.pc.set_trace(w);
        self.vga_fast_path = false;
    }

    /// Call `f` with a freshly rendered frame 60 times a second while running.
    pub fn set_display(&mut self, f: DisplayFn) {
        self.display = Some(f);
    }

    /// [`Machine::run`] returns [`Stop::Quit`] soon after `flag` becomes true.
    pub fn set_stop_flag(&mut self, flag: Arc<AtomicBool>) {
        self.stop_flag = Some(flag);
    }

    /// Render the display as it is now.
    pub fn render(&mut self) -> &Frame {
        let now = self.now();
        self.renderer.render(self.pc.vga(), now, &mut self.frame);
        &self.frame
    }

    pub fn memory(&self) -> &GuestMemory {
        &self.mem
    }

    pub fn reached(&self) -> &[Milestone] {
        &self.reached
    }

    fn now(&self) -> Nanos {
        self.start.elapsed().as_nanos() as Nanos
    }

    /// Put the vCPU in the x86 power-on state QEMU uses: CS:IP = F000:FFF0
    /// with CS base 0xFFFF0000, EDX = the CPUID(1) signature.
    fn reset_vcpu(&mut self) -> Result<()> {
        let cs = WHV_REGISTER_VALUE {
            Segment: WHV_X64_SEGMENT_REGISTER {
                Base: 0xFFFF_0000,
                Limit: 0xFFFF,
                Selector: 0xF000,
                Anonymous: WHV_X64_SEGMENT_REGISTER_0 { Attributes: 0x9B },
            },
        };
        let names = [
            WHvX64RegisterCs,
            WHvX64RegisterRip,
            WHvX64RegisterRflags,
            WHvX64RegisterRdx,
        ];
        let values = [
            cs,
            WHV_REGISTER_VALUE { Reg64: 0xFFF0 },
            WHV_REGISTER_VALUE { Reg64: 0x2 },
            WHV_REGISTER_VALUE { Reg64: self.cpuid.query(1, 0, 0).eax as u64 },
        ];
        self.part.set_regs(VP, &names, &values)?;
        Ok(())
    }

    /// Run until `until` is reached, `limit` of wall time passes, or the
    /// guest asks for a reset.
    pub fn run(&mut self, until: Option<Milestone>, limit: Option<Duration>) -> Result<Stop> {
        let deadline = limit.map(|l| Instant::now() + l);
        let debug = std::env::var_os("TEMPLEOS_DEBUG").is_some();
        let log_exits = std::env::var("TEMPLEOS_DEBUG").is_ok_and(|v| v == "exits");
        let mut counts = std::collections::BTreeMap::<i32, u64>::new();
        let mut last_report = Instant::now();
        loop {
            if debug && last_report.elapsed() >= Duration::from_secs(1) {
                last_report = Instant::now();
                let rip = self.part.get_reg(VP, WHvX64RegisterRip).map(|v| unsafe { v.Reg64 }).unwrap_or(0);
                eprintln!("[debug] exits {counts:?} rip={rip:#x} pic_int={}", self.pc.has_interrupt());
            }
            let now = self.now();
            self.pc.poll(now);
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
                self.renderer.render(self.pc.vga(), now, &mut self.frame);
                if let Some(show) = &mut self.display {
                    show(&self.frame);
                }
            }
            let wake = self.next_wake(now);

            if let Some(interrupts_enabled) = self.halted {
                if !(interrupts_enabled && self.pc.has_interrupt()) {
                    // HLT with nothing to deliver: sleep until the next timer
                    // event or frame, then look again.
                    self.timer.sleep(Duration::from_nanos(wake.saturating_sub(now)));
                    continue;
                }
                self.halted = None;
            }

            self.update_vga_mapping()?;
            self.inject_interrupts()?;
            self.kicker.arm(wake);
            let exit = self.part.run(VP);
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
                self.reach(Milestone::LongMode);
            }

            match exit.ExitReason {
                WHvRunVpExitReasonMemoryAccess => self.emulate_mmio(&exit)?,
                WHvRunVpExitReasonX64IoPortAccess => self.emulate_io(&exit)?,
                WHvRunVpExitReasonX64Cpuid => self.cpuid_exit(&exit)?,
                WHvRunVpExitReasonX64Halt => self.halted = Some(exit.VpContext.Rflags & 0x200 != 0),
                WHvRunVpExitReasonX64InterruptWindow => {
                    self.ready_for_pic = true;
                    self.window_registered = false;
                }
                WHvRunVpExitReasonCanceled | WHvRunVpExitReasonX64ApicEoi => {}
                other => {
                    return Err(Error(format!(
                        "vCPU stopped: exit reason {:#x}\n{}",
                        other.0,
                        self.dump_regs()
                    )))
                }
            }
        }
    }

    fn check_events(&mut self, until: Option<Milestone>) -> Option<Stop> {
        let out = self.pc.take_debugcon();
        if !out.is_empty() {
            self.debugcon(&out);
        }
        if self.pc.kernel_timer_programmed() && self.reached.contains(&Milestone::LongMode) {
            self.reach(Milestone::KernelTimers);
        }
        if self.pc.take_reset_request() {
            return Some(Stop::ResetRequested);
        }
        until.filter(|m| self.reached.contains(m)).map(Stop::Milestone)
    }

    fn reach(&mut self, m: Milestone) {
        if !self.reached.contains(&m) {
            self.reached.push(m);
            let t = self.start.elapsed().as_secs_f64();
            println!("[vmm {t:8.3}s] milestone: {}", m.describe());
        }
    }

    fn debugcon(&mut self, bytes: &[u8]) {
        if let Some(w) = &mut self.debugcon_sink {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
        for &b in bytes {
            if b != b'\n' {
                self.debugcon_line.push(b);
                continue;
            }
            let line = String::from_utf8_lossy(&self.debugcon_line).into_owned();
            self.debugcon_line.clear();
            if self.echo_debugcon {
                println!("[debugcon] {line}");
            }
            if line.starts_with("SeaBIOS (version") {
                self.reach(Milestone::BiosBanner);
            }
        }
    }

    /// Before entering the guest: hand it a pending 8259 interrupt if the
    /// last exit opened an interrupt window, otherwise ask for a window.
    fn inject_interrupts(&mut self) -> Result<()> {
        let ready = std::mem::take(&mut self.ready_for_pic);
        if !self.pc.has_interrupt() {
            return Ok(());
        }
        if ready {
            let vector = self.pc.acknowledge_interrupt();
            // WHV_X64_PENDING_EXT_INT_EVENT: EventPending (bit 0), EventType (bits 1-3), Vector (bits 8-15).
            let event = 1u64 | (WHvX64PendingEventExtInt.0 as u64) << 1 | (vector as u64) << 8;
            let value = WHV_REGISTER_VALUE { Reg128: WHV_UINT128 { Anonymous: WHV_UINT128_0 { Low64: event, High64: 0 } } };
            self.part.set_regs(VP, &[WHvRegisterPendingEvent], &[value])?;
        } else if !self.window_registered {
            // WHV_X64_DELIVERABILITY_NOTIFICATIONS_REGISTER: InterruptNotification is bit 1.
            let value = WHV_REGISTER_VALUE { Reg64: 1 << 1 };
            self.part.set_regs(VP, &[WHvX64RegisterDeliverabilityNotifications], &[value])?;
            self.window_registered = true;
        }
        Ok(())
    }

    /// When the run loop must next look at the machine: the next device
    /// timer, the next frame, and at most 10 ms from now.
    fn next_wake(&self, now: Nanos) -> Nanos {
        let mut wake = now + 10_000_000;
        if let Some(t) = self.pc.next_deadline() {
            wake = wake.min(t);
        }
        if self.display.is_some() {
            wake = wake.min(self.next_frame);
        }
        wake
    }

    /// Map or unmap a VGA plane at 0xA0000 to follow the VGA's state. Runs
    /// before every guest entry; the VGA registers only change in exits.
    fn update_vga_mapping(&mut self) -> Result<()> {
        let want = if self.vga_fast_path { self.pc.vga_fast_plane() } else { None };
        if want.map(|(p, _)| p) == self.vga_mapped {
            return Ok(());
        }
        let window = VGA_HOLE.start;
        let len = 0x10000;
        if self.vga_mapped.take().is_some() {
            self.part.unmap(window, len)?;
        }
        if let Some((plane, ptr)) = want {
            // SAFETY: the plane is a page-aligned 4 MiB block owned by the
            // VGA, which lives as long as the machine; the host only touches
            // it while the vCPU is stopped.
            unsafe { self.part.map(ptr, window, len, Access::ReadWrite)? };
            self.vga_mapped = Some(plane);
        }
        Ok(())
    }

    fn cpuid_exit(&mut self, exit: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<()> {
        // SAFETY: the exit reason says this union member is the active one.
        let c = unsafe { exit.Anonymous.CpuidAccess };
        let r = self.cpuid.query(c.Rax as u32, c.Rcx as u32, 0);
        let len = (exit.VpContext._bitfield & 0xF) as u64;
        let names = [
            WHvX64RegisterRax,
            WHvX64RegisterRbx,
            WHvX64RegisterRcx,
            WHvX64RegisterRdx,
            WHvX64RegisterRip,
        ];
        let values = [r.eax as u64, r.ebx as u64, r.ecx as u64, r.edx as u64, exit.VpContext.Rip + len]
            .map(|v| WHV_REGISTER_VALUE { Reg64: v });
        self.part.set_regs(VP, &names, &values)?;
        Ok(())
    }

    fn emulate_io(&mut self, exit: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<()> {
        let emulator = self.emulator.handle;
        let mut ctx = EmuContext::new(self);
        // SAFETY: the exit reason says IoPortAccess is the active union member;
        // ctx outlives the call.
        let status = unsafe {
            WHvEmulatorTryIoEmulation(
                emulator,
                &mut ctx as *mut EmuContext as _,
                &exit.VpContext,
                &exit.Anonymous.IoPortAccess,
            )
        };
        self.check_emulation("I/O", status, exit)
    }

    fn emulate_mmio(&mut self, exit: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<()> {
        let emulator = self.emulator.handle;
        let mut ctx = EmuContext::new(self);
        // SAFETY: as in emulate_io, with MemoryAccess active.
        let status = unsafe {
            WHvEmulatorTryMmioEmulation(
                emulator,
                &mut ctx as *mut EmuContext as _,
                &exit.VpContext,
                &exit.Anonymous.MemoryAccess,
            )
        };
        self.check_emulation("MMIO", status, exit)
    }

    fn check_emulation(
        &self,
        what: &str,
        status: windows::core::Result<WHV_EMULATOR_STATUS>,
        exit: &WHV_RUN_VP_EXIT_CONTEXT,
    ) -> Result<()> {
        let status = status.map_err(|e| Error(format!("{what} emulation call failed: {e}")))?;
        // SAFETY: bitfield over a u32; bit 0 is EmulationSuccessful.
        let bits = unsafe { status.AsUINT32 };
        if bits & 1 == 0 {
            // SAFETY: reading the exit union for diagnostics only.
            let detail = unsafe {
                match exit.ExitReason {
                    WHvRunVpExitReasonX64IoPortAccess => {
                        format!("port {:#x}", exit.Anonymous.IoPortAccess.PortNumber)
                    }
                    _ => format!("gpa {:#x}", exit.Anonymous.MemoryAccess.Gpa),
                }
            };
            return Err(Error(format!(
                "{what} emulation failed at {detail} (status {bits:#x})\n{}",
                self.dump_regs()
            )));
        }
        Ok(())
    }

    /// A register dump for error reports.
    pub fn dump_regs(&self) -> String {
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
            match self.part.get_reg(VP, *name) {
                // SAFETY: all of these are 64-bit registers.
                Ok(v) => out += &format!("{label:>6}={:016x}", unsafe { v.Reg64 }),
                Err(_) => out += &format!("{label:>6}=?"),
            }
            out.push(if i % 4 == 3 { '\n' } else { ' ' });
        }
        if let Ok(cs) = self.part.get_reg(VP, WHvX64RegisterCs) {
            // SAFETY: CS is a segment register.
            let s = unsafe { cs.Segment };
            out += &format!("\n    cs={:04x} base={:x} attr={:04x}", s.Selector, s.Base, unsafe {
                s.Anonymous.Attributes
            });
        }
        out
    }
}

/// State the instruction emulator's callbacks need during one emulation.
struct EmuContext<'a> {
    part: &'a Partition,
    mem: &'a mut GuestMemory,
    pc: &'a mut Pc,
    now: Nanos,
}

impl<'a> EmuContext<'a> {
    fn new(m: &'a mut Machine) -> Self {
        let now = m.now();
        EmuContext { part: &m.part, mem: &mut m.mem, pc: &mut m.pc, now }
    }
}

/// The WHPX instruction emulator (decodes the exiting I/O or MMIO
/// instruction and calls back into us for the device access).
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
    let ctx = &mut *(ctx as *mut EmuContext);
    let io = &mut *io;
    let size = io.AccessSize as u8;
    if io.Direction == 0 {
        io.Data = ctx.pc.io_read(io.Port, size, ctx.now);
    } else {
        ctx.pc.io_write(io.Port, size, io.Data, ctx.now, ctx.mem);
    }
    S_OK
}

unsafe extern "system" fn emu_memory(
    ctx: *const core::ffi::c_void,
    ma: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO,
) -> HRESULT {
    use devices::GuestMemory as _;
    let ctx = &mut *(ctx as *mut EmuContext);
    let ma = &mut *ma;
    let size = (ma.AccessSize as usize).min(8);
    let data = &mut ma.Data[..size];
    if ma.Direction == 0 {
        if !ctx.mem.read(ma.GpaAddress, data) {
            let v = ctx.pc.mmio_read(ma.GpaAddress, size as u8, ctx.now);
            data.copy_from_slice(&v.to_le_bytes()[..size]);
        }
    } else if !ctx.mem.write(ma.GpaAddress, data) {
        let mut buf = [0u8; 8];
        buf[..size].copy_from_slice(data);
        ctx.pc.mmio_write(ma.GpaAddress, size as u8, u64::from_le_bytes(buf), ctx.now);
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
    match ctx.part.get_regs(VP, names, values) {
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
    match ctx.part.set_regs(VP, names, values) {
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
    match ctx.part.translate_gva(VP, gva, flags) {
        Ok((code, addr)) => {
            *result = code;
            *gpa = addr;
            S_OK
        }
        Err(_) => E_FAIL,
    }
}

/// Kicks the vCPU out of the guest when the next device deadline arrives
/// (the vCPU thread is blocked inside WHvRunVirtualProcessor until then).
struct Kicker {
    /// Deadline in ns since machine start; u64::MAX when disarmed.
    deadline: Arc<AtomicU64>,
}

impl Kicker {
    fn spawn(part: Arc<Partition>, start: Instant) -> Self {
        let deadline = Arc::new(AtomicU64::new(u64::MAX));
        let d = deadline.clone();
        std::thread::Builder::new()
            .name("vcpu-kicker".into())
            .spawn(move || {
                let timer = HrTimer::new();
                loop {
                    let now = start.elapsed().as_nanos() as u64;
                    let due = d.load(Ordering::Acquire);
                    if due != u64::MAX && now >= due {
                        if d.compare_exchange(due, u64::MAX, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                            part.cancel(VP);
                        }
                        continue;
                    }
                    // Re-check at least every millisecond so a newly armed,
                    // earlier deadline is noticed.
                    timer.sleep(Duration::from_nanos(due.saturating_sub(now).min(1_000_000)));
                }
            })
            .expect("spawn kicker thread");
        Kicker { deadline }
    }

    fn arm(&self, at: Nanos) {
        self.deadline.store(at, Ordering::Release);
    }

    fn disarm(&self) {
        self.deadline.store(u64::MAX, Ordering::Release);
    }
}
