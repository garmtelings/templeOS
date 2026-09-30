//! Guest-physical memory: RAM and the BIOS flash, laid out like QEMU's
//! i440FX `pc` machine (docs/hw-surface.md §9).
//!
//! | Range                      | Backing                                      |
//! |----------------------------|----------------------------------------------|
//! | 0 .. 0xA0000               | RAM                                          |
//! | 0xA0000 .. 0xC0000         | not mapped: the VGA window, emulated by the   |
//! |                            | board (or one VGA plane, mapped by the VMM)   |
//! | 0xC0000 .. ram_size        | RAM (including 0xC0000-0xFFFFF, see below)    |
//! | 0xFFFC0000 .. 0x1_00000000 | BIOS flash, read/execute only                 |
//!
//! The RAM allocation still covers 0xA0000-0xBFFFF; those bytes are unused.
//!
//! Simplification, invisible to SeaBIOS and TempleOS:
//! - 0xC0000-0xFFFFF is always RAM. QEMU routes it through the i440FX PAM
//!   registers (reads from flash until SeaBIOS enables shadow RAM, read-only
//!   after POST). We start it with the flash's top 128 KiB copied to 0xE0000,
//!   so reads return what QEMU's would; the only difference is that writes
//!   after POST aren't discarded, and nothing writes there.

use crate::whpx::{Access, Partition, Result};
use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};

pub const BIOS_SIZE: usize = 256 << 10;
pub const BIOS_BASE: u64 = (1 << 32) - BIOS_SIZE as u64;
/// Top of the RAM we support below the PCI hole (QEMU's i440FX lowmem limit).
pub const MAX_LOW_RAM: u64 = 0xE000_0000;
/// The legacy VGA window, which is never RAM.
pub const VGA_HOLE: std::ops::Range<u64> = devices::vga::WINDOW_BASE..devices::vga::WINDOW_BASE + devices::vga::WINDOW_SIZE;

/// A page-aligned, zero-filled host allocation that can be mapped into a guest.
pub struct HostBuf {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: HostBuf owns its allocation exclusively.
unsafe impl Send for HostBuf {}

impl HostBuf {
    pub fn new(len: usize) -> Self {
        // SAFETY: fresh allocation; checked for failure below.
        let ptr = unsafe { VirtualAlloc(None, len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        assert!(!ptr.is_null(), "out of memory allocating {len} bytes of guest memory");
        HostBuf { ptr: ptr as *mut u8, len }
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr..ptr+len is a live allocation owned by self.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, and &mut self gives exclusive access on the host side.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for HostBuf {
    fn drop(&mut self) {
        // SAFETY: ptr came from VirtualAlloc and is released exactly once.
        let _ = unsafe { VirtualFree(self.ptr as _, 0, MEM_RELEASE) };
    }
}

pub struct GuestMemory {
    ram: HostBuf,
    bios: HostBuf,
}

// SAFETY: guest memory is shared by the vCPU threads, as it is by the vCPUs
// themselves. Host-side writes go through raw pointers
// ([`GuestMemory::write_shared`]); no Rust reference into it is held across
// a guest run.
unsafe impl Sync for GuestMemory {}

/// A `devices::GuestMemory` view that writes through a shared reference
/// (fw_cfg DMA and the instruction emulator, on any vCPU thread).
pub struct MemRef<'a>(pub &'a GuestMemory);

impl devices::GuestMemory for MemRef<'_> {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        devices::GuestMemory::read(self.0, gpa, buf)
    }

    fn write(&mut self, gpa: u64, data: &[u8]) -> bool {
        self.0.write_shared(gpa, data)
    }
}

impl GuestMemory {
    pub fn new(ram_size: u64, bios: &[u8]) -> Self {
        assert_eq!(bios.len(), BIOS_SIZE, "BIOS image must be {BIOS_SIZE} bytes");
        assert!(ram_size >= 1 << 20 && ram_size <= MAX_LOW_RAM && ram_size % 4096 == 0);
        let mut ram = HostBuf::new(ram_size as usize);
        let mut rom = HostBuf::new(BIOS_SIZE);
        rom.as_mut_slice().copy_from_slice(bios);
        ram.as_mut_slice()[0xE0000..0x100000].copy_from_slice(&bios[BIOS_SIZE - 0x20000..]);
        GuestMemory { ram, bios: rom }
    }

    /// Map RAM and the BIOS flash into `part`.
    pub fn map_into(&mut self, part: &Partition) -> Result<()> {
        // SAFETY: both buffers live as long as self, which the machine keeps
        // alive for the partition's lifetime; the host only touches them while
        // the vCPU is stopped in an exit.
        unsafe {
            part.map(self.ram.ptr, 0, VGA_HOLE.start, Access::ReadWriteExecute)?;
            part.map(
                self.ram.ptr.add(VGA_HOLE.end as usize),
                VGA_HOLE.end,
                self.ram.len as u64 - VGA_HOLE.end,
                Access::ReadWriteExecute,
            )?;
            part.map(self.bios.ptr, BIOS_BASE, BIOS_SIZE as u64, Access::ReadExecute)?;
        }
        Ok(())
    }

    pub fn ram(&self) -> &[u8] {
        self.ram.as_slice()
    }

    pub fn ram_size(&self) -> u64 {
        self.ram.len() as u64
    }

    /// True if `gpa..gpa+len` is backed by RAM or flash (never MMIO).
    pub fn is_memory(&self, gpa: u64, len: usize) -> bool {
        self.slice(gpa, len).is_some()
    }

    fn slice(&self, gpa: u64, len: usize) -> Option<(&HostBuf, usize)> {
        let end = gpa.checked_add(len as u64)?;
        if gpa < VGA_HOLE.end && end > VGA_HOLE.start {
            None
        } else if end <= self.ram.len() as u64 {
            Some((&self.ram, gpa as usize))
        } else if gpa >= BIOS_BASE && end <= 1 << 32 {
            Some((&self.bios, (gpa - BIOS_BASE) as usize))
        } else {
            None
        }
    }
}

impl GuestMemory {
    /// Write guest RAM from any thread: what a DMA-ing device would do.
    /// Writes to the flash are dropped; the VGA window is not memory.
    pub fn write_shared(&self, gpa: u64, data: &[u8]) -> bool {
        let end = gpa.saturating_add(data.len() as u64);
        if gpa < VGA_HOLE.end && end > VGA_HOLE.start {
            false
        } else if end <= self.ram.len() as u64 {
            // SAFETY: in bounds of the RAM allocation, which lives as long as
            // self; concurrent guest access is the same race real DMA has.
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ram.ptr.add(gpa as usize), data.len()) };
            true
        } else {
            gpa >= BIOS_BASE && end <= 1 << 32
        }
    }
}

impl devices::GuestMemory for GuestMemory {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        match self.slice(gpa, buf.len()) {
            Some((b, off)) => {
                buf.copy_from_slice(&b.as_slice()[off..off + buf.len()]);
                true
            }
            None => false,
        }
    }

    /// Writes to the flash are dropped, like writes to a ROM. The VGA window
    /// is not memory.
    fn write(&mut self, gpa: u64, data: &[u8]) -> bool {
        let end = gpa.saturating_add(data.len() as u64);
        if gpa < VGA_HOLE.end && end > VGA_HOLE.start {
            false
        } else if end <= self.ram.len() as u64 {
            let off = gpa as usize;
            self.ram.as_mut_slice()[off..off + data.len()].copy_from_slice(data);
            true
        } else {
            gpa >= BIOS_BASE && end <= 1 << 32
        }
    }
}
