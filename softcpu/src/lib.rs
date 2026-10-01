//! Raw bindings to the software CPU (glue/softcpu.h): Bochs's x86-64 CPU core
//! with memory, port I/O, the 8259's interrupt line and CPUID answered by
//! the host. See vmm's software machine for the safe use of it.

use std::ffi::{c_char, c_int, c_void};

#[repr(C)]
pub struct Host {
    pub ctx: *mut c_void,
    pub io_read: unsafe extern "C" fn(ctx: *mut c_void, port: u16, len: u32) -> u32,
    pub io_write: unsafe extern "C" fn(ctx: *mut c_void, port: u16, len: u32, value: u32),
    pub mmio_read: unsafe extern "C" fn(ctx: *mut c_void, addr: u64, len: u32) -> u64,
    pub mmio_write: unsafe extern "C" fn(ctx: *mut c_void, addr: u64, len: u32, value: u64),
    pub iac: unsafe extern "C" fn(ctx: *mut c_void) -> u8,
    pub cpuid: unsafe extern "C" fn(ctx: *mut c_void, cpu: u32, leaf: u32, subleaf: u32, out: *mut u32),
    pub deadline: unsafe extern "C" fn(ctx: *mut c_void),
    pub log: unsafe extern "C" fn(ctx: *mut c_void, level: c_int, msg: *const c_char),
}

extern "C" {
    pub fn softcpu_init(host: *const Host, ncpus: u32, ram: *mut u8, ram_size: u64, bios: *const u8, bios_size: u32) -> c_int;
    pub fn softcpu_run(ticks: u64) -> c_int;
    pub fn softcpu_ticks() -> u64;
    pub fn softcpu_set_deadline(tick: u64);
    pub fn softcpu_set_intr(level: c_int);
    pub fn softcpu_mem_written(addr: u64, len: u64);
    pub fn softcpu_advance(ticks: u64);
    pub fn softcpu_stop();
    pub fn softcpu_all_idle() -> c_int;
    pub fn softcpu_long_mode(cpu: u32) -> c_int;
    pub fn softcpu_waiting_for_sipi(cpu: u32) -> c_int;
    pub fn softcpu_rip(cpu: u32) -> u64;
    pub fn softcpu_reset_requested() -> c_int;
    pub fn softcpu_error() -> *const c_char;
}
