//! QEMU fw_cfg (`hw/nvram/fw_cfg.c`), I/O port interface with DMA, as the
//! pc machine exposes it:
//!
//! - 0x510 (16-bit write): selector. Byte reads at 0x510 or 0x511 return the
//!   next data byte of the selected item (QEMU's combined region).
//! - 0x511 (8-bit read): data. Byte writes are ignored (legacy write support
//!   was removed in QEMU 2.4); other sizes are invalid (read 0).
//! - 0x514-0x51B: big-endian 64-bit DMA address. A 32-bit write to 0x514
//!   sets the high half (and clears the low half), a 32-bit write to 0x518
//!   ORs in the low half and starts the transfer. Reads return the bytes of
//!   "QEMU CFG".
//!
//! Items are byte strings addressed by a 16-bit key. Bit 15 selects the
//! architecture-local table (0x8000+), bit 14 (write channel) is ignored;
//! keys whose index is 0x40 or above select nothing and read as zeros.

use crate::GuestMemory;

pub const FW_CFG_PORT_SEL: u16 = 0x510;
pub const FW_CFG_PORT_DATA: u16 = 0x511;
pub const FW_CFG_PORT_DMA: u16 = 0x514;

pub const FW_CFG_SIGNATURE: u16 = 0x00;
pub const FW_CFG_ID: u16 = 0x01;
pub const FW_CFG_UUID: u16 = 0x02;
pub const FW_CFG_RAM_SIZE: u16 = 0x03;
pub const FW_CFG_NOGRAPHIC: u16 = 0x04;
pub const FW_CFG_NB_CPUS: u16 = 0x05;
pub const FW_CFG_BOOT_MENU: u16 = 0x0E;
pub const FW_CFG_MAX_CPUS: u16 = 0x0F;
pub const FW_CFG_FILE_DIR: u16 = 0x19;
pub const FW_CFG_FILE_FIRST: u16 = 0x20;
/// Number of file slots (QEMU's default `x-file-slots`).
pub const FW_CFG_FILE_SLOTS: usize = 0x20;
pub const FW_CFG_ARCH_LOCAL: u16 = 0x8000;
const FW_CFG_WRITE_CHANNEL: u16 = 0x4000;
const FW_CFG_ENTRY_MASK: u16 = !(FW_CFG_WRITE_CHANNEL | FW_CFG_ARCH_LOCAL);
const MAX_ENTRY: usize = FW_CFG_FILE_FIRST as usize + FW_CFG_FILE_SLOTS;

/// FW_CFG_ID: traditional interface + DMA.
pub const FW_CFG_VERSION: u32 = 0x01 | 0x02;

/// "QEMU CFG", read back from the DMA address registers.
pub const FW_CFG_DMA_SIGNATURE: u64 = 0x5145_4d55_2043_4647;

/// FWCfgDmaAccess.control bits.
pub mod dma_ctl {
    pub const ERROR: u32 = 0x01;
    pub const READ: u32 = 0x02;
    pub const SKIP: u32 = 0x04;
    pub const SELECT: u32 = 0x08;
    pub const WRITE: u32 = 0x10;
}

/// Size of one directory entry: be32 size, be16 select, be16 reserved, name[56].
const FILE_ENTRY_SIZE: usize = 64;
const FILE_NAME_SIZE: usize = 56;

/// QEMU fw_cfg, I/O port interface with DMA.
pub struct FwCfg {
    /// `[arch][index]` item data; `None` means no item.
    entries: [Vec<Option<Vec<u8>>>; 2],
    /// Files in directory order (sorted by name).
    files: Vec<(String, Vec<u8>)>,
    /// Selected key with the write-channel bit removed; `None` = invalid.
    cur_entry: Option<u16>,
    cur_offset: usize,
    dma_addr: u64,
}

impl Default for FwCfg {
    fn default() -> Self {
        Self::new()
    }
}

impl FwCfg {
    /// Signature "QEMU" (key 0x00), ID (key 0x01) = 3 (traditional + DMA),
    /// file dir (key 0x19), plus what QEMU's `fw_cfg_common_realize` always
    /// adds: an all-zero UUID (0x02), NOGRAPHIC = 0 (0x04), BOOT_MENU = 0
    /// (0x0E) and the file "etc/boot-fail-wait" = -1 (reboot timeout unset).
    /// All of these can be replaced with `add_bytes` / `add_file`.
    pub fn new() -> Self {
        let mut f = FwCfg {
            entries: [vec![None; MAX_ENTRY], vec![None; MAX_ENTRY]],
            files: Vec::new(),
            cur_entry: None,
            cur_offset: 0,
            dma_addr: 0,
        };
        f.add_bytes(FW_CFG_SIGNATURE, b"QEMU".to_vec());
        f.add_bytes(FW_CFG_UUID, vec![0; 16]);
        f.add_bytes(FW_CFG_NOGRAPHIC, 0u16.to_le_bytes().to_vec());
        f.add_bytes(FW_CFG_BOOT_MENU, 0u16.to_le_bytes().to_vec());
        f.add_bytes(FW_CFG_ID, FW_CFG_VERSION.to_le_bytes().to_vec());
        f.add_file("etc/boot-fail-wait", (-1i32).to_le_bytes().to_vec());
        f
    }

    fn slot(key: u16) -> (usize, usize) {
        let arch = (key & FW_CFG_ARCH_LOCAL != 0) as usize;
        (arch, (key & FW_CFG_ENTRY_MASK) as usize)
    }

    /// Set item `key` (e.g. 0x05 NB_CPUS as u16 LE, 0x0F MAX_CPUS, 0x03
    /// RAM_SIZE u64 LE, 0x0E BOOT_MENU; 0x8000+ for arch-local items).
    /// Panics if `key` is outside the 0x40-entry tables.
    pub fn add_bytes(&mut self, key: u16, data: Vec<u8>) {
        let (arch, index) = Self::slot(key);
        assert!(index < MAX_ENTRY, "fw_cfg key {key:#x} out of range");
        self.entries[arch][index] = Some(data);
    }

    /// Add a named file. The directory is kept sorted by name like QEMU, and
    /// selectors are assigned from 0x20 in directory order, so adding a file
    /// can move the selectors of files after it. Adding an existing name
    /// replaces its contents (QEMU's `fw_cfg_modify_file`). Names are
    /// truncated to 55 bytes. Panics when all 32 slots are used.
    pub fn add_file(&mut self, name: &str, data: Vec<u8>) {
        let mut name = name.to_string();
        while name.len() >= FILE_NAME_SIZE {
            name.pop();
        }
        match self.files.binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes())) {
            Ok(i) => self.files[i].1 = data,
            Err(i) => {
                assert!(self.files.len() < FW_CFG_FILE_SLOTS, "fw_cfg file slots exhausted");
                self.files.insert(i, (name, data));
            }
        }
        self.rebuild_directory();
    }

    /// Selector of file `name`, if present.
    pub fn file_selector(&self, name: &str) -> Option<u16> {
        self.files.iter().position(|(n, _)| n == name).map(|i| FW_CFG_FILE_FIRST + i as u16)
    }

    fn rebuild_directory(&mut self) {
        let mut dir = vec![0u8; 4 + FILE_ENTRY_SIZE * FW_CFG_FILE_SLOTS];
        dir[..4].copy_from_slice(&(self.files.len() as u32).to_be_bytes());
        for (i, (name, data)) in self.files.iter().enumerate() {
            let select = FW_CFG_FILE_FIRST + i as u16;
            let e = &mut dir[4 + i * FILE_ENTRY_SIZE..][..FILE_ENTRY_SIZE];
            e[0..4].copy_from_slice(&(data.len() as u32).to_be_bytes());
            e[4..6].copy_from_slice(&select.to_be_bytes());
            e[8..8 + name.len()].copy_from_slice(name.as_bytes());
            self.entries[0][select as usize] = Some(data.clone());
        }
        self.entries[0][FW_CFG_FILE_DIR as usize] = Some(dir);
    }

    fn select(&mut self, key: u16) {
        self.cur_offset = 0;
        let (_, index) = Self::slot(key);
        self.cur_entry = (index < MAX_ENTRY).then_some(key & !FW_CFG_WRITE_CHANNEL);
    }

    fn current(&self) -> Option<&[u8]> {
        let (arch, index) = Self::slot(self.cur_entry?);
        self.entries[arch][index].as_deref()
    }

    /// QEMU `fw_cfg_data_read`: the next `size` bytes as a big-endian number,
    /// zero-padded on the right past the end of the item.
    fn data_read(&mut self, size: u8) -> u64 {
        let offset = self.cur_offset;
        let Some(data) = self.current() else { return 0 };
        if offset >= data.len() {
            return 0;
        }
        let n = (size as usize).min(data.len() - offset);
        let v = data[offset..offset + n].iter().fold(0u64, |v, b| v << 8 | *b as u64);
        self.cur_offset += n;
        v << (8 * (size as usize - n))
    }

    /// 0x510 (selector), 0x511 (data), 0x514-0x51B (DMA address; reads return
    /// the "QEMU CFG" signature bytes as QEMU does).
    pub fn io_read(&mut self, port: u16, size: u8) -> u32 {
        match port {
            0x510..=0x511 => {
                if size == 1 {
                    self.data_read(1) as u32
                } else {
                    0
                }
            }
            0x514..=0x51B => {
                let sig = FW_CFG_DMA_SIGNATURE.to_be_bytes();
                let start = (port - FW_CFG_PORT_DMA) as usize;
                (0..size as usize)
                    .map(|i| sig.get(start + i).copied().unwrap_or(0))
                    .rev()
                    .fold(0u32, |v, b| v << 8 | b as u32)
            }
            _ => {
                crate::unhandled("fw_cfg io read", port as u64, size, None);
                0xFFFF_FFFF
            }
        }
    }

    /// DMA: big-endian 64-bit address, the write to the low half (0x518)
    /// starts it; FWCfgDmaAccess {control BE32, length BE32, address BE64};
    /// READ/SKIP/SELECT/WRITE bits, error bit, control cleared on
    /// completion, as QEMU.
    pub fn io_write(&mut self, port: u16, size: u8, val: u32, mem: &mut dyn GuestMemory) {
        match (port, size) {
            (0x510, 2) => self.select(val as u16),
            (0x510..=0x511, 1) => {} // legacy data writes are ignored
            (0x514, 4) => self.dma_addr = (val.swap_bytes() as u64) << 32,
            (0x518, 4) => {
                self.dma_addr |= val.swap_bytes() as u64;
                self.dma_transfer(mem);
            }
            (0x510..=0x511 | 0x514..=0x51B, _) => {} // invalid access, dropped
            _ => crate::unhandled("fw_cfg io write", port as u64, size, Some(val as u64)),
        }
    }

    /// QEMU `fw_cfg_dma_transfer`.
    fn dma_transfer(&mut self, mem: &mut dyn GuestMemory) {
        let desc = std::mem::take(&mut self.dma_addr);
        let mut raw = [0u8; 16];
        if !mem.read(desc, &mut raw) {
            mem.write(desc, &dma_ctl::ERROR.to_be_bytes());
            return;
        }
        let control = u32::from_be_bytes(raw[0..4].try_into().unwrap());
        let mut length = u32::from_be_bytes(raw[4..8].try_into().unwrap());
        let mut address = u64::from_be_bytes(raw[8..16].try_into().unwrap());

        if control & dma_ctl::SELECT != 0 {
            self.select((control >> 16) as u16);
        }
        let (read, write) = if control & dma_ctl::READ != 0 {
            (true, false)
        } else if control & dma_ctl::WRITE != 0 {
            (false, true)
        } else if control & dma_ctl::SKIP != 0 {
            (false, false)
        } else {
            length = 0;
            (false, false)
        };

        let mut status = 0u32;
        while length > 0 && status & dma_ctl::ERROR == 0 {
            let offset = self.cur_offset;
            let available = self.current().map_or(0, |d| d.len().saturating_sub(offset));
            let len;
            if available == 0 {
                // Past the end (or no item): reads fill zeros, writes fail.
                len = length;
                if read && !fill_zero(mem, address, len as u64) {
                    status |= dma_ctl::ERROR;
                }
                if write {
                    status |= dma_ctl::ERROR;
                }
            } else {
                len = length.min(available as u32);
                if read {
                    let data = self.current().unwrap();
                    if !mem.write(address, &data[offset..offset + len as usize]) {
                        status |= dma_ctl::ERROR;
                    }
                }
                if write {
                    // No item on this machine is guest-writable.
                    status |= dma_ctl::ERROR;
                }
                self.cur_offset += len as usize;
            }
            address = address.wrapping_add(len as u64);
            length -= len;
        }
        mem.write(desc, &status.to_be_bytes());
    }
}

/// Zero `len` bytes of guest memory in bounded chunks.
fn fill_zero(mem: &mut dyn GuestMemory, addr: u64, len: u64) -> bool {
    const CHUNK: u64 = 4096;
    let zeros = [0u8; CHUNK as usize];
    let mut done = 0;
    let mut ok = true;
    while done < len {
        let n = (len - done).min(CHUNK);
        ok &= mem.write(addr.wrapping_add(done), &zeros[..n as usize]);
        done += n;
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram(Vec<u8>);

    impl GuestMemory for Ram {
        fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
            let Some(src) = self.0.get(gpa as usize..gpa as usize + buf.len()) else { return false };
            buf.copy_from_slice(src);
            true
        }
        fn write(&mut self, gpa: u64, data: &[u8]) -> bool {
            let Some(dst) = self.0.get_mut(gpa as usize..gpa as usize + data.len()) else { return false };
            dst.copy_from_slice(data);
            true
        }
    }

    fn read_item(f: &mut FwCfg, key: u16, len: usize, ram: &mut Ram) -> Vec<u8> {
        f.io_write(0x510, 2, key as u32, ram);
        (0..len).map(|_| f.io_read(0x511, 1) as u8).collect()
    }

    fn dma(f: &mut FwCfg, ram: &mut Ram, desc: u64, control: u32, len: u32, addr: u64) -> u32 {
        let mut d = Vec::new();
        d.extend_from_slice(&control.to_be_bytes());
        d.extend_from_slice(&len.to_be_bytes());
        d.extend_from_slice(&addr.to_be_bytes());
        ram.write(desc, &d);
        // SeaBIOS: outl(cpu_to_be32(addr), 0x518)
        f.io_write(0x518, 4, (desc as u32).swap_bytes(), ram);
        u32::from_be_bytes(ram.0[desc as usize..][..4].try_into().unwrap())
    }

    #[test]
    fn signature_and_id() {
        let mut f = FwCfg::new();
        let mut ram = Ram(vec![0; 16]);
        assert_eq!(read_item(&mut f, 0, 4, &mut ram), b"QEMU");
        assert_eq!(f.io_read(0x511, 1), 0, "past the end reads 0");
        assert_eq!(read_item(&mut f, 1, 4, &mut ram), [3, 0, 0, 0]);
        // Byte reads at the selector port also return data; wider reads are invalid.
        f.io_write(0x510, 2, 0, &mut ram);
        assert_eq!(f.io_read(0x510, 1), b'Q' as u32);
        assert_eq!(f.io_read(0x511, 2), 0);
        // Unknown and out-of-range keys read zeros.
        assert_eq!(read_item(&mut f, 0x3F, 2, &mut ram), [0, 0]);
        assert_eq!(read_item(&mut f, 0x1234, 2, &mut ram), [0, 0]);
        // The write-channel bit is ignored.
        assert_eq!(read_item(&mut f, 0x4000, 4, &mut ram), b"QEMU");
        // DMA registers read back the "QEMU CFG" bytes.
        assert_eq!(f.io_read(0x514, 4).to_le_bytes(), *b"QEMU");
        assert_eq!(f.io_read(0x518, 4).to_le_bytes(), *b" CFG");
        assert_eq!(f.io_read(0x515, 1), b'E' as u32);
    }

    #[test]
    fn items_and_arch_local() {
        let mut f = FwCfg::new();
        let mut ram = Ram(vec![0; 16]);
        f.add_bytes(FW_CFG_NB_CPUS, 1u16.to_le_bytes().to_vec());
        f.add_bytes(FW_CFG_RAM_SIZE, (1u64 << 30).to_le_bytes().to_vec());
        f.add_bytes(0x8002, vec![1, 0, 0, 0]);
        assert_eq!(read_item(&mut f, 5, 2, &mut ram), [1, 0]);
        assert_eq!(read_item(&mut f, 3, 8, &mut ram), (1u64 << 30).to_le_bytes());
        assert_eq!(read_item(&mut f, 0x8002, 4, &mut ram), [1, 0, 0, 0]);
        assert_eq!(read_item(&mut f, 0x0002, 4, &mut ram), [0, 0, 0, 0]);
    }

    #[test]
    fn file_directory_format() {
        let mut f = FwCfg::new();
        let mut ram = Ram(vec![0; 16]);
        f.add_file("etc/e820", vec![0xAA; 20]);
        f.add_file("bootorder", b"/pci\0".to_vec());
        f.add_file("genroms/kvmvapic.bin", vec![1, 2, 3]);
        let dir = read_item(&mut f, 0x19, 4 + 64 * 32 + 1, &mut ram);
        assert_eq!(dir.len(), 4 + 64 * 32 + 1);
        assert_eq!(*dir.last().unwrap(), 0, "directory is 2052 bytes");
        assert_eq!(&dir[..4], &4u32.to_be_bytes());
        let names = ["bootorder", "etc/boot-fail-wait", "etc/e820", "genroms/kvmvapic.bin"];
        let sizes = [5u32, 4, 20, 3];
        for (i, (name, size)) in names.iter().zip(sizes).enumerate() {
            let e = &dir[4 + 64 * i..][..64];
            assert_eq!(&e[0..4], &size.to_be_bytes());
            assert_eq!(&e[4..6], &(0x20 + i as u16).to_be_bytes());
            assert_eq!(&e[6..8], &[0, 0]);
            assert_eq!(&e[8..8 + name.len()], name.as_bytes());
            assert!(e[8 + name.len()..].iter().all(|&b| b == 0));
            assert_eq!(f.file_selector(name), Some(0x20 + i as u16));
        }
        assert_eq!(read_item(&mut f, 0x21, 4, &mut ram), [0xFF; 4]);
        assert_eq!(read_item(&mut f, 0x22, 2, &mut ram), [0xAA; 2]);
    }

    #[test]
    fn dma_read_file() {
        let mut f = FwCfg::new();
        let mut ram = Ram(vec![0x55; 0x2000]);
        let contents: Vec<u8> = (0..100).collect();
        f.add_file("etc/test", contents.clone());
        let sel = f.file_selector("etc/test").unwrap() as u32;
        let ctl = dma_ctl::SELECT | dma_ctl::READ | sel << 16;
        assert_eq!(dma(&mut f, &mut ram, 0x100, ctl, 60, 0x1000), 0);
        assert_eq!(&ram.0[0x1000..0x103C], &contents[..60]);
        assert_eq!(ram.0[0x103C], 0x55);
        // Skip 10, then read past the end: the tail is zero-filled.
        assert_eq!(dma(&mut f, &mut ram, 0x100, dma_ctl::SKIP, 10, 0), 0);
        assert_eq!(dma(&mut f, &mut ram, 0x100, dma_ctl::READ, 40, 0x1100), 0);
        assert_eq!(&ram.0[0x1100..0x111E], &contents[70..]);
        assert!(ram.0[0x111E..0x1128].iter().all(|&b| b == 0));
        assert_eq!(ram.0[0x1128], 0x55);
        // Writes fail with the error bit.
        let ctl = dma_ctl::SELECT | dma_ctl::WRITE | sel << 16;
        assert_eq!(dma(&mut f, &mut ram, 0x100, ctl, 4, 0x1000), dma_ctl::ERROR);
        // A destination outside RAM sets the error bit.
        let ctl = dma_ctl::SELECT | dma_ctl::READ | sel << 16;
        assert_eq!(dma(&mut f, &mut ram, 0x100, ctl, 4, 0x10_0000), dma_ctl::ERROR);
        // High-half address write then low half.
        let mut d = Vec::new();
        d.extend_from_slice(&(dma_ctl::SELECT | dma_ctl::READ).to_be_bytes());
        d.extend_from_slice(&4u32.to_be_bytes());
        d.extend_from_slice(&0x1200u64.to_be_bytes());
        ram.write(0x180, &d);
        f.io_write(0x514, 4, 0, &mut ram);
        f.io_write(0x518, 4, 0x180u32.swap_bytes(), &mut ram);
        assert_eq!(&ram.0[0x1200..0x1204], b"QEMU");
        assert_eq!(&ram.0[0x180..0x184], &[0; 4]);
    }
}
