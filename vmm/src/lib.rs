//! The virtual machine monitor: runs the PC board from the `devices` crate
//! on the Windows Hypervisor Platform (see PLAN.md).

pub mod cpuid;
pub mod machine;
pub mod memory;
pub mod timer;
pub mod whpx;

pub use machine::{Config, Machine, Milestone, Stop};
