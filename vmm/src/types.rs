//! Types shared by the hypervisor machine ([`crate::machine`], Windows) and
//! the software-CPU machine ([`crate::soft`], any OS).

use devices::vga_render::Frame;
use devices::Nanos;

pub const MAX_CPUS: u32 = 64;

pub struct Config {
    pub ram_size: u64,
    pub bios: &'static [u8],
    pub vgabios: &'static [u8],
    pub cdrom: Option<&'static [u8]>,
    /// Hard disk on the primary IDE master.
    pub hdd: Option<Box<dyn devices::ide::DiskImage>>,
    /// Guest wall-clock time at power-on, in Unix seconds.
    pub rtc_base: i64,
    /// Map a VGA plane straight into the guest while that is exact
    /// ([`devices::vga::Vga::fast_plane`]). Off means every access to the
    /// VGA window is emulated.
    pub vga_fast_path: bool,
    /// Number of vCPUs, 1 to [`MAX_CPUS`].
    pub cpus: u32,
    /// Software CPU only: guest time is the instruction count (1 ns per
    /// instruction, as QEMU's `-icount shift=0`) and never follows the host
    /// clock, so runs repeat exactly. Off, guest time keeps up with the
    /// host's, as with the hypervisor.
    pub exact_time: bool,
}

/// Receives each rendered frame (see `set_display`).
pub type DisplayFn = Box<dyn FnMut(&Frame)>;

/// Told the PC speaker's tone whenever it changes (None = silent).
pub type SpeakerFn = Box<dyn FnMut(Option<f64>)>;

/// Guest time between rendered frames.
pub(crate) const FRAME_NS: Nanos = 1_000_000_000 / 60;

/// Points in the boot the run loop recognizes and reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Milestone {
    /// SeaBIOS printed its version banner on the debug console.
    BiosBanner,
    /// The vCPU is in long mode (EFER.LMA) for the first time.
    LongMode,
    /// The kernel programmed PIT channel 0 for its 1 kHz tick (TimersInit).
    KernelTimers,
    /// An application processor was started (SIPI) for the first time.
    ApStarted,
}

impl Milestone {
    pub fn describe(self) -> &'static str {
        match self {
            Milestone::BiosBanner => "SeaBIOS banner on the debug console",
            Milestone::LongMode => "guest entered long mode",
            Milestone::KernelTimers => "kernel programmed the PIT (TimersInit)",
            Milestone::ApStarted => "an application processor was started",
        }
    }
}

/// Why `run` returned.
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
