//! Device models for the TempleOS machine: an i440FX + PIIX3 PC, reduced to
//! what SeaBIOS and TempleOS touch (docs/hw-surface.md).
//!
//! Everything here is plain Rust with no hypervisor dependency, so the models
//! can be unit tested anywhere. The VMM owns a [`pc::Pc`] and forwards port
//! I/O, MMIO and the passage of time to it.
//!
//! Conventions shared by all devices:
//! - Time is [`Nanos`]: nanoseconds since the machine was created, supplied
//!   by the caller. Devices never read a clock themselves, which keeps them
//!   deterministic under test.
//! - Behavior follows QEMU 8.2 (the reference machine) wherever the guest
//!   can observe it. Where the model is deliberately simpler, the code says so.
//! - Accesses a model doesn't implement go through [`unhandled`], never a
//!   silent default.

/// Nanoseconds since machine creation.
pub type Nanos = u64;

/// Guest-physical memory as seen by a device doing DMA (only fw_cfg does).
/// Accesses outside RAM fail and return false, leaving `buf` untouched.
pub trait GuestMemory {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool;
    fn write(&mut self, gpa: u64, data: &[u8]) -> bool;
}

// While the device models are developed in parallel, each group can be built
// alone with `RUSTFLAGS='--cfg dev_only="<group>"'`. A normal build includes
// everything.
#[cfg(any(not(any(dev_only = "timers", dev_only = "ide", dev_only = "chipset", dev_only = "vga")), dev_only = "timers"))]
pub mod pic;
#[cfg(any(not(any(dev_only = "timers", dev_only = "ide", dev_only = "chipset", dev_only = "vga")), dev_only = "timers"))]
pub mod pit;
#[cfg(any(not(any(dev_only = "timers", dev_only = "ide", dev_only = "chipset", dev_only = "vga")), dev_only = "timers"))]
pub mod rtc;

#[cfg(any(not(any(dev_only = "timers", dev_only = "ide", dev_only = "chipset", dev_only = "vga")), dev_only = "ide"))]
pub mod ide;

#[cfg(any(not(any(dev_only = "timers", dev_only = "ide", dev_only = "chipset", dev_only = "vga")), dev_only = "chipset"))]
pub mod acpi;
#[cfg(any(not(any(dev_only = "timers", dev_only = "ide", dev_only = "chipset", dev_only = "vga")), dev_only = "chipset"))]
pub mod fwcfg;
#[cfg(any(not(any(dev_only = "timers", dev_only = "ide", dev_only = "chipset", dev_only = "vga")), dev_only = "chipset"))]
pub mod hpet;
#[cfg(any(not(any(dev_only = "timers", dev_only = "ide", dev_only = "chipset", dev_only = "vga")), dev_only = "chipset"))]
pub mod pci;

#[cfg(any(not(any(dev_only = "timers", dev_only = "ide", dev_only = "chipset", dev_only = "vga")), dev_only = "vga"))]
pub mod dma;
#[cfg(any(not(any(dev_only = "timers", dev_only = "ide", dev_only = "chipset", dev_only = "vga")), dev_only = "vga"))]
pub mod ps2;
#[cfg(any(not(any(dev_only = "timers", dev_only = "ide", dev_only = "chipset", dev_only = "vga")), dev_only = "vga"))]
pub mod vga;

#[cfg(not(any(dev_only = "timers", dev_only = "ide", dev_only = "chipset", dev_only = "vga")))]
pub mod pc;

use std::collections::HashMap;
use std::sync::Mutex;

/// Report an access no model handles. Each distinct (kind, address) is
/// printed the first time it happens, then counted silently; [`unhandled_report`]
/// lists the counts.
///
/// `kind` is a short tag such as "io read", "mmio write" or "ide cmd".
pub fn unhandled(kind: &str, addr: u64, size: u8, value: Option<u64>) {
    let mut seen = UNHANDLED.lock().unwrap();
    let map = seen.get_or_insert_with(HashMap::new);
    let n = map.entry((kind.to_string(), addr)).or_insert(0);
    *n += 1;
    if *n == 1 {
        match value {
            Some(v) => eprintln!("[unhandled] {kind} {addr:#x} size {size} value {v:#x}"),
            None => eprintln!("[unhandled] {kind} {addr:#x} size {size}"),
        }
    }
}

/// Counts of every unhandled access so far, most frequent first.
pub fn unhandled_report() -> Vec<(String, u64, u64)> {
    let seen = UNHANDLED.lock().unwrap();
    let mut v: Vec<_> = seen
        .iter()
        .flat_map(|m| m.iter())
        .map(|((k, a), n)| (k.clone(), *a, *n))
        .collect();
    v.sort_by(|a, b| b.2.cmp(&a.2));
    v
}

static UNHANDLED: Mutex<Option<HashMap<(String, u64), u64>>> = Mutex::new(None);
