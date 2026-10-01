//! The virtual machine monitor: runs the PC board from the `devices` crate,
//! on the Windows Hypervisor Platform ([`machine`]) or, where that can't be
//! used, on a software CPU ([`soft`]; see PLAN.md).
//!
//! The hypervisor parts need Windows; the software machine and the rest run
//! anywhere, so `cargo test -p vmm` boots TempleOS on any OS.

pub mod bitop;
pub mod cpuid;
mod events;
#[cfg(windows)]
pub mod machine;
#[cfg(windows)]
pub mod memory;
pub mod soft;
#[cfg(windows)]
pub mod timer;
pub mod types;
#[cfg(windows)]
pub mod whpx;

#[cfg(windows)]
pub use machine::Machine;
pub use soft::SoftMachine;
pub use types::{Config, Error, Milestone, Stop, MAX_CPUS};
