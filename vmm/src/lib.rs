//! The virtual machine monitor: runs the PC board from the `devices` crate
//! on the Windows Hypervisor Platform (see PLAN.md).
//!
//! Only [`cpuid`] and [`bitop`] are platform independent; the rest needs
//! Windows. Gating the Windows parts lets `cargo test -p vmm` run their
//! tests anywhere.

pub mod bitop;
pub mod cpuid;
#[cfg(windows)]
pub mod machine;
#[cfg(windows)]
pub mod memory;
#[cfg(windows)]
pub mod timer;
#[cfg(windows)]
pub mod whpx;

#[cfg(windows)]
pub use machine::{Config, Machine, Milestone, Stop};
