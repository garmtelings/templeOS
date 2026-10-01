//! The Bochs core runs real-mode code from the reset vector and does port
//! I/O through the host.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::Mutex;

use softcpu::*;

static OUT: Mutex<Vec<(u16, u32)>> = Mutex::new(Vec::new());

unsafe extern "C" fn io_read(_: *mut c_void, _: u16, _: u32) -> u32 {
    0xFFFF_FFFF
}
unsafe extern "C" fn io_write(_: *mut c_void, port: u16, _: u32, value: u32) {
    OUT.lock().unwrap().push((port, value));
}
unsafe extern "C" fn mmio_read(_: *mut c_void, _: u64, _: u32) -> u64 {
    !0
}
unsafe extern "C" fn mmio_write(_: *mut c_void, _: u64, _: u32, _: u64) {}
unsafe extern "C" fn iac(_: *mut c_void) -> u8 {
    0x20
}
unsafe extern "C" fn cpuid(_: *mut c_void, _: u32, _: u32, _: u32, out: *mut u32) {
    for i in 0..4 {
        *out.add(i) = 0;
    }
}
unsafe extern "C" fn deadline(_: *mut c_void) {}
unsafe extern "C" fn log(_: *mut c_void, level: c_int, msg: *const c_char) {
    if level >= 2 {
        eprintln!("{}", CStr::from_ptr(msg).to_string_lossy());
    }
}

/// A page-aligned buffer, as softcpu_init requires.
#[repr(C, align(4096))]
struct Page([u8; 4096]);

fn pages(n: usize, fill: u8) -> Vec<Page> {
    (0..n).map(|_| Page([fill; 4096])).collect()
}

fn bytes(p: &mut [Page]) -> &mut [u8] {
    // SAFETY: Page is a plain 4096-byte array; the slice covers them all.
    unsafe { std::slice::from_raw_parts_mut(p.as_mut_ptr() as *mut u8, p.len() * 4096) }
}

#[test]
fn reset_vector_runs() {
    let host = Host { ctx: std::ptr::null_mut(), io_read, io_write, mmio_read, mmio_write, iac, cpuid, deadline, log };
    let mut ram_pages = pages(256, 0);
    let mut bios_pages = pages(16, 0xF4); // HLT everywhere
    let ram = bytes(&mut ram_pages);
    let bios = bytes(&mut bios_pages);
    // F000:FFF0: mov al, 'A'; out 0xE9, al; hlt
    bios[0xFFF0..0xFFF5].copy_from_slice(&[0xB0, 0x41, 0xE6, 0xE9, 0xF4]);
    unsafe {
        assert_eq!(softcpu_init(&host, 1, ram.as_mut_ptr(), ram.len() as u64, bios.as_ptr(), bios.len() as u32), 0, "{:?}", CStr::from_ptr(softcpu_error()));
        assert_eq!(softcpu_run(10_000), 0, "{:?}", CStr::from_ptr(softcpu_error()));
        assert_eq!(*OUT.lock().unwrap(), vec![(0xE9, 0x41)]);
        assert_eq!(softcpu_all_idle(), 1, "halted");
        assert!(softcpu_ticks() >= 10_000);
        assert_eq!(softcpu_rip(0), 0xFFF5);
    }
}
