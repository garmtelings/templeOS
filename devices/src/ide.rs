//! Legacy IDE channel with ATA and ATAPI (PIO only), modelled on QEMU 8.2's
//! `hw/ide/core.c` and `hw/ide/atapi.c` as wired up by the PIIX3 in
//! compatibility mode.
//!
//! Each [`IdeChannel`] is one cable: a command block (0x1F0-0x1F7 or
//! 0x170-0x177), a control block register (0x3F6 or 0x376) and two drive
//! slots. The CD is served straight from a `&'static [u8]` ISO image; a hard
//! disk from a [`DiskImage`] (a file, or memory in tests), written through
//! on every WRITE command.
//!
//! Guest-visible behavior follows QEMU closely, including its quirks: every
//! drive keeps QEMU's persistent I/O buffer, so replies that QEMU only
//! partly fills (MODE SENSE's audio page, for example) leak the same stale
//! bytes. The one systematic difference is timing: QEMU reads CD sectors and
//! performs SRST asynchronously, so a guest can see BSY for a while; this
//! model completes that work at once, so the guest never sees BSY.
//! There is no DMA and no bus-master interface.

use std::io;

use crate::unhandled;

/// What sits in a drive slot.
pub enum Media {
    /// Read-only CD/DVD holding an ISO 9660 image (2048-byte blocks).
    Cdrom(&'static [u8]),
    /// A hard disk (512-byte sectors).
    Disk(Box<dyn DiskImage>),
}

/// Storage behind a hard disk: whole 512-byte sectors.
pub trait DiskImage: Send {
    /// Size in 512-byte sectors.
    fn sectors(&self) -> u64;
    /// Read `buf.len() / 512` sectors starting at `lba`.
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()>;
    /// Write `buf.len() / 512` sectors starting at `lba`.
    fn write(&mut self, lba: u64, buf: &[u8]) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

/// A disk image in memory (tests, or a throwaway disk).
pub struct MemDisk(pub Vec<u8>);

impl DiskImage for MemDisk {
    fn sectors(&self) -> u64 {
        self.0.len() as u64 / 512
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        let at = lba as usize * 512;
        buf.copy_from_slice(self.0.get(at..at + buf.len()).ok_or(io::ErrorKind::UnexpectedEof)?);
        Ok(())
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> io::Result<()> {
        let at = lba as usize * 512;
        self.0.get_mut(at..at + buf.len()).ok_or(io::ErrorKind::UnexpectedEof)?.copy_from_slice(buf);
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A raw disk image file. Every write goes straight to the file; FLUSH
/// CACHE also syncs it to stable storage.
pub struct FileDisk {
    file: std::fs::File,
    sectors: u64,
}

impl FileDisk {
    /// Open `path` read-write, creating it with `create_size` bytes (zero
    /// filled, sparse where the file system allows) if it doesn't exist.
    pub fn open(path: &std::path::Path, create_size: u64) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        if file.metadata()?.len() == 0 {
            file.set_len(create_size)?;
        }
        let sectors = file.metadata()?.len() / 512;
        Ok(FileDisk { file, sectors })
    }
}

impl DiskImage for FileDisk {
    fn sectors(&self) -> u64 {
        self.sectors
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        self.file.seek(SeekFrom::Start(lba * 512))?;
        self.file.read_exact(buf)
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        self.file.seek(SeekFrom::Start(lba * 512))?;
        self.file.write_all(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }
}

// Status register bits.
const ERR_STAT: u8 = 0x01;
const DRQ_STAT: u8 = 0x08;
const SEEK_STAT: u8 = 0x10;
const READY_STAT: u8 = 0x40;
const BUSY_STAT: u8 = 0x80;

// Error register bits.
const ABRT_ERR: u8 = 0x04;
const MC_ERR: u8 = 0x20;

// Device control register bits.
const CTRL_NIEN: u8 = 0x02;
const CTRL_SRST: u8 = 0x04;
const CTRL_HOB: u8 = 0x80;

// Device/head register bits.
const DEV_HS: u8 = 0x0f;
const DEV_LBA: u8 = 0x40;
const DEV_SELECT: u8 = 0x10;
const DEV_ALWAYS_ON: u8 = 0xa0;

// ATAPI interrupt reason, reported in the sector count register.
const INT_REASON_CD: u32 = 0x01;
const INT_REASON_IO: u32 = 0x02;

// SCSI sense keys.
const NO_SENSE: u8 = 0x00;
const NOT_READY: u8 = 0x02;
const ILLEGAL_REQUEST: u8 = 0x05;
const UNIT_ATTENTION: u8 = 0x06;

// Additional sense codes.
const ASC_ILLEGAL_OPCODE: u8 = 0x20;
const ASC_LOGICAL_BLOCK_OOR: u8 = 0x21;
const ASC_INV_FIELD_IN_CMD_PACKET: u8 = 0x24;
const ASC_INCOMPATIBLE_FORMAT: u8 = 0x30;
const ASC_SAVING_PARAMETERS_NOT_SUPPORTED: u8 = 0x39;
const ASC_MEDIUM_NOT_PRESENT: u8 = 0x3a;
const ASC_DATA_PHASE_ERROR: u8 = 0x4b;
const ASC_MEDIA_REMOVAL_PREVENTED: u8 = 0x53;

// ATAPI command table flags (QEMU's `atapi_cmd_table`).
const ALLOW_UA: u8 = 0x01;
const CHECK_READY: u8 = 0x02;
const NONDATA: u8 = 0x04;
const CONDDATA: u8 = 0x08;

/// Size of QEMU's per-drive I/O buffer (`IDE_DMA_BUF_SECTORS * 512 + 4`).
const IO_BUFFER_LEN: usize = 256 * 512 + 4;
const ATAPI_PACKET_SIZE: usize = 12;
const CD_SECTOR: usize = 2048;
/// QEMU's CD_MAX_SECTORS, in 512-byte units: larger media report as DVD.
const CD_MAX_SECTORS: u64 = 80 * 60 * 75 * 2048 / 512;
const MMC_PROFILE_DVD_ROM: u16 = 0x0010;
const MMC_PROFILE_CD_ROM: u16 = 0x0008;
/// QEMU_HW_VERSION, reported as firmware revision.
const FIRMWARE_VERSION: &str = "2.5+";
const CD_MODEL: &str = "QEMU DVD-ROM";
const HD_MODEL: &str = "QEMU HARDDISK";
/// QEMU's MAX_MULT_SECTORS: READ/WRITE MULTIPLE block size after reset.
const MAX_MULT_SECTORS: u8 = 16;

/// BIOS disk translation hints (QEMU `BIOS_ATA_TRANSLATION_*`), reported in
/// CMOS register 0x39.
pub const BIOS_ATA_TRANSLATION_NONE: u8 = 1;
pub const BIOS_ATA_TRANSLATION_LBA: u8 = 2;
pub const BIOS_ATA_TRANSLATION_LARGE: u8 = 3;

/// What happens when the current PIO data transfer runs out (QEMU's
/// `end_transfer_func`). The variant also fixes the transfer direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EndTransfer {
    /// Nothing in flight since reset (`ide_dummy_transfer_stop`); device to host.
    Dummy,
    /// Last block of a data-in command (`ide_transfer_stop`); device to host.
    Stop,
    /// The 12-byte ATAPI packet (`ide_atapi_cmd`); host to device.
    AtapiPacket,
    /// Next ATAPI data-in chunk (`ide_atapi_cmd_reply_end`); device to host.
    AtapiReplyEnd,
    /// Next block of a disk read (`ide_sector_read`); device to host.
    SectorRead,
    /// Block of a disk write is complete (`ide_sector_write`); host to device.
    SectorWrite,
}

impl EndTransfer {
    /// QEMU's `ide_is_pio_out`: true when data flows to the host.
    fn is_data_in(self) -> bool {
        !matches!(self, EndTransfer::AtapiPacket | EndTransfer::SectorWrite)
    }
}

/// A hard disk's CHS geometry and BIOS translation, as QEMU's
/// `hd_geometry_guess` picks them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub cylinders: u32,
    pub heads: u32,
    pub sectors: u32,
    pub translation: u8,
}

impl Geometry {
    /// QEMU `hd_geometry_guess` for an image file: a logical geometry from
    /// the MBR's partition table when there is one, else 16 heads and 63
    /// sectors per track.
    pub fn guess(disk: &mut dyn DiskImage) -> Geometry {
        let nb = disk.sectors();
        let for_size = || {
            let cyls = (nb / (16 * 63)).clamp(2, 16383) as u32;
            (cyls, 16, 63)
        };
        match guess_disk_lchs(disk) {
            None => {
                let (c, h, s) = for_size();
                let t = if c <= 1024 && h <= 16 && s <= 63 { BIOS_ATA_TRANSLATION_NONE } else { BIOS_ATA_TRANSLATION_LBA };
                Geometry { cylinders: c, heads: h, sectors: s, translation: t }
            }
            Some((_, heads, _)) if heads > 16 => {
                let (c, h, s) = for_size();
                let t = if c * h <= 131072 { BIOS_ATA_TRANSLATION_LARGE } else { BIOS_ATA_TRANSLATION_LBA };
                Geometry { cylinders: c, heads: h, sectors: s, translation: t }
            }
            Some((c, h, s)) => Geometry { cylinders: c, heads: h, sectors: s, translation: BIOS_ATA_TRANSLATION_NONE },
        }
    }
}

/// QEMU `guess_disk_lchs`: heads and sectors from the first partition entry
/// with a size and a non-zero end head.
fn guess_disk_lchs(disk: &mut dyn DiskImage) -> Option<(u32, u32, u32)> {
    let mut mbr = [0u8; 512];
    disk.read(0, &mut mbr).ok()?;
    if mbr[510] != 0x55 || mbr[511] != 0xaa {
        return None;
    }
    for i in 0..4 {
        let p = &mbr[0x1be + 16 * i..0x1be + 16 * (i + 1)];
        let nr_sects = u32::from_le_bytes([p[12], p[13], p[14], p[15]]);
        let end_head = p[5];
        if nr_sects != 0 && end_head != 0 {
            let heads = u32::from(end_head) + 1;
            let sectors = u32::from(p[6] & 63);
            if sectors == 0 {
                continue;
            }
            let cylinders = disk.sectors() / u64::from(heads * sectors);
            if !(1..=16383).contains(&cylinders) {
                continue;
            }
            return Some((cylinders as u32, heads, sectors));
        }
    }
    None
}

/// State of the channel shared by both drives: the device control register
/// and the INTRQ line.
struct Bus {
    devctl: u8,
    irq: bool,
}

impl Bus {
    /// QEMU's `ide_bus_set_irq`: nIEN only gates new assertions; it never
    /// drops a line that is already up.
    fn set_irq(&mut self) {
        if self.devctl & CTRL_NIEN == 0 {
            self.irq = true;
        }
    }
}

/// One drive slot and its register file (QEMU's `IDEState`).
struct Drive {
    media: Option<Media>,
    serial: String,

    feature: u8,
    error: u8,
    /// Sector count. Like QEMU's `int nsector`, it holds the full count
    /// (up to 65536) once a command has combined it with the HOB byte;
    /// the register reads its low byte.
    nsector: u32,
    sector: u8,
    lcyl: u8,
    hcyl: u8,
    hob_feature: u8,
    hob_nsector: u8,
    hob_sector: u8,
    hob_lcyl: u8,
    hob_hcyl: u8,
    select: u8,
    status: u8,

    identify: [u16; 256],
    identify_set: bool,

    io_buffer: Vec<u8>,
    data_ptr: usize,
    data_end: usize,
    end_transfer: EndTransfer,

    // ATAPI state.
    sense_key: u8,
    asc: u8,
    /// Next CD sector to read, or `None` for a reply built in the buffer.
    lba: Option<i64>,
    packet_transfer_size: usize,
    elementary_transfer_size: usize,
    io_buffer_index: usize,
    cd_sector_size: usize,
    tray_open: bool,
    tray_locked: bool,

    // Hard disk state.
    lba48: bool,
    /// Sectors per DRQ block of the current command.
    req_nb_sectors: u32,
    mult_sectors: u8,
    /// Current (INITIALIZE DEVICE PARAMETERS) and native geometry.
    cylinders: u32,
    heads: u32,
    sectors: u32,
    drive_heads: u32,
    drive_sectors: u32,
    /// BIOS translation hint for CMOS 0x39.
    translation: u8,
}

impl Drive {
    fn new(media: Option<Media>, serial_number: usize) -> Self {
        let mut d = Drive {
            media,
            serial: format!("QM{serial_number:05}"),
            feature: 0,
            error: 0,
            nsector: 0,
            sector: 0,
            lcyl: 0,
            hcyl: 0,
            hob_feature: 0,
            hob_nsector: 0,
            hob_sector: 0,
            hob_lcyl: 0,
            hob_hcyl: 0,
            select: DEV_ALWAYS_ON,
            status: 0,
            identify: [0; 256],
            identify_set: false,
            io_buffer: vec![0; IO_BUFFER_LEN],
            data_ptr: 0,
            data_end: 0,
            end_transfer: EndTransfer::Dummy,
            sense_key: 0,
            asc: 0,
            lba: None,
            packet_transfer_size: 0,
            elementary_transfer_size: 0,
            io_buffer_index: 0,
            cd_sector_size: 0,
            tray_open: false,
            tray_locked: false,
            lba48: false,
            req_nb_sectors: 0,
            mult_sectors: 0,
            cylinders: 0,
            heads: 0,
            sectors: 0,
            drive_heads: 0,
            drive_sectors: 0,
            translation: 0,
        };
        if let Some(Media::Disk(disk)) = &mut d.media {
            let g = Geometry::guess(disk.as_mut());
            d.cylinders = g.cylinders;
            d.heads = g.heads;
            d.sectors = g.sectors;
            d.drive_heads = g.heads;
            d.drive_sectors = g.sectors;
            d.translation = g.translation;
        }
        d.reset();
        d
    }

    fn is_disk(&self) -> bool {
        matches!(self.media, Some(Media::Disk(_)))
    }

    fn present(&self) -> bool {
        self.media.is_some()
    }

    fn is_cd(&self) -> bool {
        matches!(self.media, Some(Media::Cdrom(_)))
    }

    /// Medium size in 512-byte sectors (QEMU's `nb_sectors`).
    fn nb_sectors(&self) -> u64 {
        match &self.media {
            Some(Media::Cdrom(img)) => img.len() as u64 / 512,
            Some(Media::Disk(d)) => d.sectors(),
            None => 0,
        }
    }

    /// Medium size in 2048-byte blocks.
    fn total_cd_sectors(&self) -> u64 {
        self.nb_sectors() >> 2
    }

    fn media_present(&self) -> bool {
        !self.tray_open && self.nb_sectors() > 0
    }

    /// QEMU's `ide_reset`: power-on and SRST/DEVICE RESET register state.
    /// The tray lock and tray position are left alone, as in QEMU.
    fn reset(&mut self) {
        self.feature = 0;
        self.error = 0;
        self.nsector = 0;
        self.sector = 0;
        self.lcyl = 0;
        self.hcyl = 0;
        self.hob_feature = 0;
        self.hob_sector = 0;
        self.hob_nsector = 0;
        self.hob_lcyl = 0;
        self.hob_hcyl = 0;
        self.select = DEV_ALWAYS_ON;
        self.status = READY_STAT | SEEK_STAT;
        self.sense_key = 0;
        self.asc = 0;
        self.packet_transfer_size = 0;
        self.elementary_transfer_size = 0;
        self.io_buffer_index = 0;
        self.cd_sector_size = 0;
        self.mult_sectors = MAX_MULT_SECTORS;
        self.set_signature();
        self.end_transfer = EndTransfer::Dummy;
        self.data_ptr = 0;
        self.data_end = 0;
        self.io_buffer[..4].fill(0xff);
    }

    /// Device signature: packet devices 01/01/14/EB, disks 01/01/00/00 and
    /// an empty slot 01/01/FF/FF.
    fn set_signature(&mut self) {
        self.select &= !DEV_HS;
        self.nsector = 1;
        self.sector = 1;
        let (lcyl, hcyl) = match self.media {
            Some(Media::Cdrom(_)) => (0x14, 0xeb),
            Some(Media::Disk(_)) => (0, 0),
            None => (0xff, 0xff),
        };
        self.lcyl = lcyl;
        self.hcyl = hcyl;
    }

    /// QEMU's `ide_transfer_halt`.
    fn transfer_halt(&mut self) {
        self.end_transfer = EndTransfer::Stop;
        self.data_ptr = 0;
        self.data_end = 0;
        self.status &= !DRQ_STAT;
    }

    /// QEMU's `ide_transfer_stop`. It raises no interrupt: the host reading
    /// the last word of a data-in block does not get an IRQ.
    fn transfer_stop(&mut self) {
        self.transfer_halt();
    }

    /// Opens a PIO data window over `io_buffer[start..start + len]`.
    fn transfer_start(&mut self, start: usize, len: usize, end: EndTransfer) {
        self.data_ptr = start;
        self.data_end = start + len;
        if self.status & ERR_STAT == 0 {
            self.status |= DRQ_STAT;
        }
        self.end_transfer = end;
    }

    fn abort_command(&mut self) {
        self.transfer_stop();
        self.status = READY_STAT | ERR_STAT;
        self.error = ABRT_ERR;
    }

    fn end_of_transfer(&mut self, bus: &mut Bus) {
        match self.end_transfer {
            EndTransfer::Dummy | EndTransfer::Stop => self.transfer_stop(),
            EndTransfer::AtapiPacket => self.atapi_command(bus),
            EndTransfer::AtapiReplyEnd => self.atapi_reply_end(bus),
            EndTransfer::SectorRead => self.sector_read(bus),
            EndTransfer::SectorWrite => self.sector_write(bus),
        }
    }

    /// QEMU's `ide_data_read{w,l}`. Outside a data-in phase the read
    /// returns 0 and moves nothing. When fewer than `width` bytes are left
    /// (an odd-sized ATAPI reply) it also returns 0 without advancing, so
    /// the transfer can only be finished with narrower reads, as in QEMU.
    fn data_read(&mut self, bus: &mut Bus, width: usize) -> u32 {
        if self.status & DRQ_STAT == 0 || !self.end_transfer.is_data_in() {
            return 0;
        }
        let p = self.data_ptr;
        if p + width > self.data_end {
            return 0;
        }
        let mut bytes = [0u8; 4];
        bytes[..width].copy_from_slice(&self.io_buffer[p..p + width]);
        self.data_ptr = p + width;
        if self.data_ptr >= self.data_end {
            self.status &= !DRQ_STAT;
            self.end_of_transfer(bus);
        }
        u32::from_le_bytes(bytes)
    }

    /// QEMU's `ide_data_write{w,l}`: ignored outside a data-out phase.
    fn data_write(&mut self, bus: &mut Bus, width: usize, val: u32) {
        if self.status & DRQ_STAT == 0 || self.end_transfer.is_data_in() {
            return;
        }
        let p = self.data_ptr;
        if p + width > self.data_end {
            return;
        }
        self.io_buffer[p..p + width].copy_from_slice(&val.to_le_bytes()[..width]);
        self.data_ptr = p + width;
        if self.data_ptr >= self.data_end {
            self.status &= !DRQ_STAT;
            self.end_of_transfer(bus);
        }
    }

    // ---- ATA commands ----------------------------------------------------

    /// Runs an ATA command on this (present or absent) drive, following
    /// QEMU's `ide_bus_exec_cmd` and `ide_cmd_table`.
    fn ata_command(&mut self, bus: &mut Bus, cmd: u8) {
        if self.status & (BUSY_STAT | DRQ_STAT) != 0 && !(cmd == 0x08 && self.is_cd()) {
            return;
        }
        let Some(set_dsc) = self.ata_permitted(cmd) else {
            if !qemu_known_ata_command(cmd) {
                unhandled("ide cmd", cmd as u64, 1, None);
            }
            self.abort_command();
            bus.set_irq();
            return;
        };

        self.status = READY_STAT | BUSY_STAT;
        self.error = 0;
        if self.is_disk() {
            if self.disk_command(bus, cmd) {
                self.status &= !BUSY_STAT;
                if set_dsc && self.error == 0 {
                    self.status |= SEEK_STAT;
                }
                bus.set_irq();
            }
            return;
        }
        let complete = match cmd {
            0x08 => self.cmd_device_reset(),
            0x20 | 0xec => {
                // READ SECTORS and IDENTIFY DEVICE: a packet device aborts
                // with its signature in the task file (ATA4 8.27.5.2).
                if self.is_cd() {
                    self.set_signature();
                }
                self.abort_command();
                true
            }
            0x90 => self.cmd_exec_dev_diagnostic(bus),
            0xa0 => self.cmd_packet(),
            0xa1 => self.cmd_identify_packet(bus),
            // FLUSH CACHE: nothing to flush on a read-only image.
            0xe7 => {
                self.status = READY_STAT | SEEK_STAT;
                true
            }
            0xef => self.cmd_set_features(),
            _ => unreachable!("ata_permitted admitted {cmd:#x}"),
        };
        if complete {
            self.status &= !BUSY_STAT;
            if set_dsc && self.error == 0 {
                self.status |= SEEK_STAT;
            }
            bus.set_irq();
        }
    }

    /// Whether this drive accepts `cmd`; `Some(set_dsc)` if it does.
    ///
    /// An empty slot is only reachable as the master of a channel whose
    /// status reads 0 whatever happens, so apart from EXECUTE DEVICE
    /// DIAGNOSTIC (which sets its signature) it simply aborts. QEMU runs
    /// its HD handlers there, which also end with an IRQ; only the hidden
    /// status value differs.
    fn ata_permitted(&self, cmd: u8) -> Option<bool> {
        match (&self.media, cmd) {
            (Some(Media::Cdrom(_)), 0x08 | 0xa0 | 0xa1 | 0x20 | 0xe7 | 0xec) => Some(false),
            (Some(Media::Cdrom(_)), 0xef) => Some(true),
            // QEMU's HD_OK entries without SET_DSC ...
            (
                Some(Media::Disk(_)),
                0x20 | 0x21 | 0x24 | 0x29 | 0x30 | 0x31 | 0x34 | 0x39 | 0x3c | 0x94 | 0x95 | 0x96
                | 0x97 | 0x99 | 0xc4 | 0xc5 | 0xe0 | 0xe1 | 0xe2 | 0xe3 | 0xe6 | 0xe7 | 0xea | 0xec,
            ) => Some(false),
            // ... and with it.
            (
                Some(Media::Disk(_)),
                0x10 | 0x27 | 0x40 | 0x41 | 0x42 | 0x70 | 0x91 | 0x98 | 0xc6 | 0xe5 | 0xef | 0xf8,
            ) => Some(true),
            (_, 0x90) => Some(false),
            _ => None,
        }
    }

    // ---- hard disk (QEMU hw/ide/core.c) -----------------------------------

    /// Runs a command `ata_permitted` admitted for a disk. Returns true when
    /// it completed (the caller raises the IRQ), false when it opened a data
    /// transfer or raised the IRQ itself.
    fn disk_command(&mut self, bus: &mut Bus, cmd: u8) -> bool {
        match cmd {
            0x20 | 0x21 | 0x24 => {
                self.lba48_transform(cmd == 0x24);
                self.req_nb_sectors = 1;
                self.sector_read(bus);
                false
            }
            0xc4 | 0x29 => {
                if self.mult_sectors == 0 {
                    self.abort_command();
                    return true;
                }
                self.lba48_transform(cmd == 0x29);
                self.req_nb_sectors = u32::from(self.mult_sectors);
                self.sector_read(bus);
                false
            }
            0x30 | 0x31 | 0x34 | 0x3c => {
                self.lba48_transform(cmd == 0x34);
                self.req_nb_sectors = 1;
                self.status = SEEK_STAT | READY_STAT;
                self.transfer_start(0, 512, EndTransfer::SectorWrite);
                false
            }
            0xc5 | 0x39 => {
                if self.mult_sectors == 0 {
                    self.abort_command();
                    return true;
                }
                self.lba48_transform(cmd == 0x39);
                self.req_nb_sectors = u32::from(self.mult_sectors);
                let n = self.nsector.min(self.req_nb_sectors) as usize;
                self.status = SEEK_STAT | READY_STAT;
                self.transfer_start(0, 512 * n, EndTransfer::SectorWrite);
                false
            }
            0x40..=0x42 => {
                self.lba48_transform(cmd == 0x42);
                true
            }
            0x27 | 0xf8 => {
                if self.nb_sectors() == 0 {
                    self.abort_command();
                } else {
                    let (h, s) = (self.heads, self.sectors);
                    self.heads = self.drive_heads;
                    self.sectors = self.drive_sectors;
                    self.lba48_transform(cmd == 0x27);
                    self.set_sector(self.nb_sectors() - 1);
                    self.heads = h;
                    self.sectors = s;
                }
                true
            }
            0x91 => {
                // INITIALIZE DEVICE PARAMETERS.
                self.heads = u32::from(self.select & DEV_HS) + 1;
                self.sectors = self.nsector;
                bus.set_irq();
                true
            }
            0xc6 => {
                let n = self.nsector & 0xff;
                if n != 0 && (n > u32::from(MAX_MULT_SECTORS) || n & (n - 1) != 0) {
                    self.abort_command();
                } else {
                    self.mult_sectors = n as u8;
                }
                true
            }
            0xe5 | 0x98 => {
                self.nsector = 0xff; // active or idle
                true
            }
            0xe7 | 0xea => {
                if let Some(Media::Disk(d)) = &mut self.media {
                    if d.flush().is_err() {
                        self.rw_error(bus);
                        return false;
                    }
                }
                self.status = READY_STAT | SEEK_STAT;
                true
            }
            0xec => {
                if !self.identify_set {
                    self.identify = hd_identify(self);
                    self.identify_set = true;
                }
                for (i, w) in self.identify.iter().enumerate() {
                    self.io_buffer[2 * i..2 * i + 2].copy_from_slice(&w.to_le_bytes());
                }
                self.status = READY_STAT | SEEK_STAT;
                self.transfer_start(0, 512, EndTransfer::Stop);
                bus.set_irq();
                false
            }
            0xef => self.cmd_set_features(),
            // RECALIBRATE, SEEK, standby/idle/sleep: nothing to do.
            _ => true,
        }
    }

    /// QEMU `ide_cmd_lba48_transform`: turn the count registers into the
    /// full sector count (0 means 256, or 65536 in LBA48).
    fn lba48_transform(&mut self, lba48: bool) {
        self.lba48 = lba48;
        if !lba48 {
            if self.nsector == 0 {
                self.nsector = 256;
            }
        } else if self.nsector == 0 && self.hob_nsector == 0 {
            self.nsector = 65536;
        } else {
            self.nsector = (u32::from(self.hob_nsector) << 8) | (self.nsector & 0xff);
        }
    }

    /// QEMU `ide_get_sector`: LBA48, LBA28 or CHS from the task file.
    fn get_sector(&self) -> i64 {
        if self.select & DEV_LBA != 0 {
            if self.lba48 {
                (i64::from(self.hob_hcyl) << 40)
                    | (i64::from(self.hob_lcyl) << 32)
                    | (i64::from(self.hob_sector) << 24)
                    | (i64::from(self.hcyl) << 16)
                    | (i64::from(self.lcyl) << 8)
                    | i64::from(self.sector)
            } else {
                (i64::from(self.select & DEV_HS) << 24)
                    | (i64::from(self.hcyl) << 16)
                    | (i64::from(self.lcyl) << 8)
                    | i64::from(self.sector)
            }
        } else {
            let cyl = (i64::from(self.hcyl) << 8) | i64::from(self.lcyl);
            cyl * i64::from(self.heads) * i64::from(self.sectors)
                + i64::from(self.select & DEV_HS) * i64::from(self.sectors)
                + (i64::from(self.sector) - 1)
        }
    }

    /// QEMU `ide_set_sector`.
    fn set_sector(&mut self, n: u64) {
        if self.select & DEV_LBA != 0 {
            if self.lba48 {
                self.sector = n as u8;
                self.lcyl = (n >> 8) as u8;
                self.hcyl = (n >> 16) as u8;
                self.hob_sector = (n >> 24) as u8;
                self.hob_lcyl = (n >> 32) as u8;
                self.hob_hcyl = (n >> 40) as u8;
            } else {
                self.select = (self.select & !DEV_HS) | ((n >> 24) as u8 & DEV_HS);
                self.hcyl = (n >> 16) as u8;
                self.lcyl = (n >> 8) as u8;
                self.sector = n as u8;
            }
        } else {
            let hs = u64::from(self.heads * self.sectors).max(1);
            let cyl = n / hs;
            let r = n % hs;
            let secs = u64::from(self.sectors).max(1);
            self.hcyl = (cyl >> 8) as u8;
            self.lcyl = cyl as u8;
            self.select = (self.select & !DEV_HS) | ((r / secs) as u8 & DEV_HS);
            // QEMU stores this in a byte: 256+ sectors per track wrap.
            self.sector = ((r % secs) + 1) as u8;
        }
    }

    fn sect_range_ok(&self, sector: i64, n: u32) -> bool {
        sector >= 0 && (sector as u64).saturating_add(u64::from(n)) <= self.nb_sectors()
    }

    /// QEMU `ide_rw_error`.
    fn rw_error(&mut self, bus: &mut Bus) {
        self.abort_command();
        bus.set_irq();
    }

    /// QEMU `ide_sector_read` + `ide_sector_read_cb`, done at once: read the
    /// next block of up to `req_nb_sectors` into the buffer and open it.
    fn sector_read(&mut self, bus: &mut Bus) {
        self.status = READY_STAT | SEEK_STAT;
        self.error = 0;
        let sector = self.get_sector();
        let n = self.nsector.min(self.req_nb_sectors);
        if self.nsector == 0 {
            self.transfer_stop();
            return;
        }
        if !self.sect_range_ok(sector, n) {
            self.rw_error(bus);
            return;
        }
        let len = n as usize * 512;
        let Some(Media::Disk(d)) = &mut self.media else { unreachable!("sector_read on a non-disk") };
        if d.read(sector as u64, &mut self.io_buffer[..len]).is_err() {
            self.rw_error(bus);
            return;
        }
        self.set_sector(sector as u64 + u64::from(n));
        self.nsector -= n;
        self.transfer_start(0, len, EndTransfer::SectorRead);
        bus.set_irq();
    }

    /// QEMU `ide_sector_write` + `ide_sector_write_cb`: the host filled a
    /// block; write it and open the next one.
    fn sector_write(&mut self, bus: &mut Bus) {
        self.status = READY_STAT | SEEK_STAT;
        let sector = self.get_sector();
        let n = self.nsector.min(self.req_nb_sectors);
        if !self.sect_range_ok(sector, n) {
            self.rw_error(bus);
            return;
        }
        let len = n as usize * 512;
        let Some(Media::Disk(d)) = &mut self.media else { unreachable!("sector_write on a non-disk") };
        if d.write(sector as u64, &self.io_buffer[..len]).is_err() {
            self.rw_error(bus);
            return;
        }
        self.nsector -= n;
        self.set_sector(sector as u64 + u64::from(n));
        if self.nsector == 0 {
            self.transfer_stop();
        } else {
            let n1 = self.nsector.min(self.req_nb_sectors) as usize;
            self.transfer_start(0, 512 * n1, EndTransfer::SectorWrite);
        }
        bus.set_irq();
    }

    fn cmd_device_reset(&mut self) -> bool {
        self.transfer_halt();
        self.reset();
        self.status = 0;
        false
    }

    /// EXECUTE DEVICE DIAGNOSTIC. Packet devices report a clear status
    /// (READY not set) and no interrupt, as QEMU does.
    fn cmd_exec_dev_diagnostic(&mut self, bus: &mut Bus) -> bool {
        self.set_signature();
        self.error = 0x01;
        if self.is_cd() {
            self.status = 0;
        } else {
            self.status = READY_STAT | SEEK_STAT;
            bus.set_irq();
        }
        false
    }

    fn cmd_packet(&mut self) -> bool {
        if self.feature & 0x02 != 0 {
            // Overlapped commands are not supported.
            self.abort_command();
            return true;
        }
        if self.feature & 0x01 != 0 {
            // Deliberate simplification: there is no bus master, so a DMA
            // packet command is carried out as PIO.
            unhandled("atapi dma", self.feature as u64, 1, None);
        }
        self.status = READY_STAT | SEEK_STAT;
        self.nsector = 1;
        self.transfer_start(0, ATAPI_PACKET_SIZE, EndTransfer::AtapiPacket);
        false
    }

    fn cmd_identify_packet(&mut self, bus: &mut Bus) -> bool {
        if !self.identify_set {
            self.identify = atapi_identify(&self.serial);
            self.identify_set = true;
        }
        for (i, w) in self.identify.iter().enumerate() {
            self.io_buffer[2 * i..2 * i + 2].copy_from_slice(&w.to_le_bytes());
        }
        self.status = READY_STAT | SEEK_STAT;
        self.transfer_start(0, 512, EndTransfer::Stop);
        bus.set_irq();
        false
    }

    /// SET FEATURES. Like QEMU, transfer mode and write cache settings edit
    /// the cached IDENTIFY data, which is only built on the first IDENTIFY
    /// PACKET DEVICE (earlier edits are lost).
    fn cmd_set_features(&mut self) -> bool {
        match self.feature {
            0x02 => self.identify[85] = (1 << 14) | (1 << 5) | 1,
            0x82 => self.identify[85] = (1 << 14) | 1,
            0xcc | 0x66 | 0xaa | 0x55 | 0x05 | 0x85 | 0x69 | 0x67 | 0x96 | 0x9a | 0x42
            | 0xc2 => {}
            0x03 => {
                let val = self.nsector & 0x07;
                let (w62, w63, w88) = match self.nsector >> 3 {
                    0x00 | 0x01 => (0x07, 0x07, 0x3f),
                    0x02 => (0x07 | (1 << (val + 8)), 0x07, 0x3f),
                    0x04 => (0x07, 0x07 | (1 << (val + 8)), 0x3f),
                    0x08 => (0x07, 0x07, 0x3f | (1 << (val + 8))),
                    _ => {
                        self.abort_command();
                        return true;
                    }
                };
                self.identify[62] = w62;
                self.identify[63] = w63;
                self.identify[88] = w88;
            }
            _ => self.abort_command(),
        }
        true
    }

    // ---- ATAPI -------------------------------------------------------------

    fn byte_count_limit(&self) -> usize {
        let bcl = self.lcyl as usize | (self.hcyl as usize) << 8;
        if bcl == 0xffff {
            0xfffe
        } else {
            bcl
        }
    }

    fn atapi_cmd_ok(&mut self, bus: &mut Bus) {
        self.error = 0;
        self.status = READY_STAT | SEEK_STAT;
        self.nsector = (self.nsector & !7) | INT_REASON_IO | INT_REASON_CD;
        self.transfer_stop();
        bus.set_irq();
    }

    /// CHECK CONDITION: status ERR, error register = sense key << 4.
    fn atapi_cmd_error(&mut self, bus: &mut Bus, sense_key: u8, asc: u8) {
        self.error = sense_key << 4;
        self.status = READY_STAT | ERR_STAT;
        self.nsector = (self.nsector & !7) | INT_REASON_IO | INT_REASON_CD;
        self.sense_key = sense_key;
        self.asc = asc;
        self.transfer_stop();
        bus.set_irq();
    }

    /// A failed medium read. The image is always present, so the only
    /// failure is reading past its end.
    fn atapi_io_error(&mut self, bus: &mut Bus) {
        self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_LOGICAL_BLOCK_OOR);
    }

    /// Dispatches the packet in `io_buffer[..12]` (QEMU's `ide_atapi_cmd`).
    fn atapi_command(&mut self, bus: &mut Bus) {
        let op = self.io_buffer[0];
        let flags = atapi_flags(op);
        let fl = flags.unwrap_or(0);

        if self.sense_key == UNIT_ATTENTION && fl & ALLOW_UA == 0 {
            self.error = MC_ERR | (UNIT_ATTENTION << 4);
            self.status = ERR_STAT;
            self.nsector = 0;
            bus.set_irq();
            return;
        }
        if fl & CHECK_READY != 0 && !self.media_present() {
            self.atapi_cmd_error(bus, NOT_READY, ASC_MEDIUM_NOT_PRESENT);
            return;
        }
        // A data command with a zero byte count limit is aborted at the ATA
        // level, without an interrupt (ATA8-ACS3 7.17.6.49).
        if flags.is_some() && fl & (NONDATA | CONDDATA) == 0 && !self.validate_bcl() {
            return;
        }

        match op {
            0x00 => self.atapi_cmd_ok(bus), // TEST UNIT READY
            0x03 => self.cmd_request_sense(bus),
            0x12 => self.cmd_inquiry(bus),
            0x1b => self.cmd_start_stop_unit(bus),
            0x1e => {
                // PREVENT ALLOW MEDIUM REMOVAL
                self.tray_locked = self.io_buffer[4] & 1 != 0;
                self.atapi_cmd_ok(bus);
            }
            0x25 => self.cmd_read_capacity(bus),
            0x28 | 0xa8 => self.cmd_read(bus),
            0x2b => self.cmd_seek(bus),
            0x43 => self.cmd_read_toc(bus),
            0x46 => self.cmd_get_configuration(bus),
            0x4a => self.cmd_get_event_status_notification(bus),
            0x51 => self.cmd_read_disc_information(bus),
            0x5a => self.cmd_mode_sense(bus),
            0xad => self.cmd_read_dvd_structure(bus),
            0xbb => self.atapi_cmd_ok(bus), // SET CD SPEED
            0xbd => self.cmd_mechanism_status(bus),
            0xbe => self.cmd_read_cd(bus),
            _ => {
                // TempleOS's DVD-burning opcodes (FORMAT UNIT, WRITE(12),
                // SYNCHRONIZE CACHE, CLOSE TRACK) and READ TRACK INFORMATION
                // are unknown to QEMU too; the answer below is the model.
                if !matches!(op, 0x04 | 0xaa | 0x35 | 0x5b | 0x52) {
                    unhandled("atapi cmd", op as u64, 1, None);
                }
                self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_ILLEGAL_OPCODE);
            }
        }
    }

    fn validate_bcl(&mut self) -> bool {
        if self.byte_count_limit() != 0 {
            return true;
        }
        self.abort_command();
        false
    }

    /// Starts a data-in reply of `size` bytes (at most `max_size`) that the
    /// handler has built at the start of `io_buffer`.
    fn atapi_cmd_reply(&mut self, bus: &mut Bus, size: usize, max_size: usize) {
        self.lba = None;
        self.packet_transfer_size = size.min(max_size);
        self.elementary_transfer_size = 0;
        self.status = READY_STAT | SEEK_STAT;
        self.io_buffer_index = 0;
        self.atapi_reply_end(bus);
    }

    /// Starts a PIO read of `nb_sectors` medium blocks of `sector_size`
    /// bytes (2048, or 2352 for raw READ CD).
    /// The byte total is computed in 32-bit signed arithmetic like QEMU's
    /// `int`, so an absurd length that wraps to zero or below completes at
    /// once without data.
    fn atapi_cmd_read(&mut self, bus: &mut Bus, lba: i64, nb_sectors: u32, sector_size: usize) {
        self.lba = Some(lba);
        let bytes = (nb_sectors as i32).wrapping_mul(sector_size as i32);
        self.packet_transfer_size = bytes.max(0) as usize;
        self.elementary_transfer_size = 0;
        self.io_buffer_index = sector_size;
        self.cd_sector_size = sector_size;
        self.atapi_reply_end(bus);
    }

    /// Opens the next data-in window (QEMU's `ide_atapi_cmd_reply_end`).
    ///
    /// Each new DRQ block ("elementary transfer") is at most the byte count
    /// limit, made even when the limit is odd and smaller than what is
    /// left; LBA mid/high report its size, the sector count reads IO=1
    /// CoD=0 and INTRQ is raised. A block may span several 2048-byte
    /// sectors: the window is refilled sector by sector with DRQ held.
    /// When all data has gone the command completes with CoD=1 IO=1.
    fn atapi_reply_end(&mut self, bus: &mut Bus) {
        if self.packet_transfer_size == 0 {
            self.atapi_cmd_ok(bus);
            return;
        }
        if self.lba.is_some() && self.io_buffer_index >= self.cd_sector_size {
            // QEMU reads the sector asynchronously (BSY meanwhile) when a
            // new block starts; here it is always immediate.
            if !self.cd_read_sector() {
                self.atapi_io_error(bus);
                return;
            }
        }
        let size = if self.elementary_transfer_size > 0 {
            (self.cd_sector_size - self.io_buffer_index).min(self.elementary_transfer_size)
        } else {
            self.nsector = (self.nsector & !7) | INT_REASON_IO;
            bus.set_irq();
            let mut limit = self.byte_count_limit();
            let mut size = self.packet_transfer_size;
            if size > limit {
                if limit & 1 != 0 {
                    limit -= 1;
                }
                size = limit;
            }
            self.lcyl = size as u8;
            self.hcyl = (size >> 8) as u8;
            self.elementary_transfer_size = size;
            if self.lba.is_some() {
                size = size.min(self.cd_sector_size - self.io_buffer_index);
            }
            size
        };
        self.packet_transfer_size -= size;
        self.elementary_transfer_size -= size;
        self.io_buffer_index += size;
        self.transfer_start(self.io_buffer_index - size, size, EndTransfer::AtapiReplyEnd);
    }

    /// Reads sector `lba` into the buffer, framed as a raw mode 1 sector
    /// when the sector size is 2352. Returns false past the end of medium.
    fn cd_read_sector(&mut self) -> bool {
        let Some(lba) = self.lba else { return false };
        let img = match self.media {
            Some(Media::Cdrom(img)) => img,
            _ => return false,
        };
        let raw = match self.cd_sector_size {
            2048 => false,
            2352 => true,
            _ => return false,
        };
        let limit = (self.nb_sectors() * 512).min(img.len() as u64);
        let Ok(start) = u64::try_from(lba).map(|l| l * CD_SECTOR as u64) else {
            return false;
        };
        if start + CD_SECTOR as u64 > limit {
            return false;
        }
        let start = start as usize;
        let off = if raw { 16 } else { 0 };
        self.io_buffer[off..off + CD_SECTOR].copy_from_slice(&img[start..start + CD_SECTOR]);
        if raw {
            // Sync pattern, MSF header and mode 1; EDC/ECC left as zeros.
            let b = &mut self.io_buffer;
            b[0] = 0;
            b[1..11].fill(0xff);
            b[11] = 0;
            b[12..15].copy_from_slice(&lba_to_msf(lba));
            b[15] = 0x01;
            b[16 + CD_SECTOR..16 + CD_SECTOR + 288].fill(0);
        }
        self.lba = Some(lba + 1);
        self.io_buffer_index = 0;
        true
    }

    /// REQUEST SENSE: fixed-format sense data. As in QEMU the sense data is
    /// sticky; only a UNIT ATTENTION is cleared by reading it.
    fn cmd_request_sense(&mut self, bus: &mut Bus) {
        let max_len = self.io_buffer[4] as usize;
        let b = &mut self.io_buffer;
        b[..18].fill(0);
        b[0] = 0x70 | 0x80;
        b[2] = self.sense_key;
        b[7] = 10;
        b[12] = self.asc;
        if self.sense_key == UNIT_ATTENTION {
            self.sense_key = NO_SENSE;
        }
        self.atapi_cmd_reply(bus, 18, max_len);
    }

    fn cmd_inquiry(&mut self, bus: &mut Bus) {
        let page_code = self.io_buffer[2];
        let max_len = self.io_buffer[4] as usize;
        let evpd = self.io_buffer[1] & 0x01 != 0;
        let serial = self.serial.clone();
        let b = &mut self.io_buffer;
        let (preamble_len, size_idx, idx);
        if evpd {
            preamble_len = 4;
            size_idx = 3;
            b[0] = 0x05;
            b[1] = page_code;
            b[2] = 0;
            let mut i = 4;
            match page_code {
                0x00 => {
                    b[4] = 0x00;
                    b[5] = 0x83;
                    i = 6;
                }
                0x83 => {
                    if i + 24 > max_len {
                        self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_DATA_PHASE_ERROR);
                        return;
                    }
                    b[i..i + 4].copy_from_slice(&[0x02, 0x00, 0x00, 20]);
                    i += 4;
                    padstr8(&mut b[i..i + 20], &serial);
                    i += 20;
                    if i + 72 <= max_len {
                        b[i..i + 4].copy_from_slice(&[0x02, 0x01, 0x00, 68]);
                        i += 4;
                        padstr8(&mut b[i..i + 8], "ATA");
                        i += 8;
                        padstr8(&mut b[i..i + 40], CD_MODEL);
                        i += 40;
                        padstr8(&mut b[i..i + 20], &serial);
                        i += 20;
                    }
                }
                _ => {
                    self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_INV_FIELD_IN_CMD_PACKET);
                    return;
                }
            }
            idx = i;
        } else {
            preamble_len = 5;
            size_idx = 4;
            b[0] = 0x05; // CD-ROM
            b[1] = 0x80; // removable
            b[2] = 0x00;
            b[3] = 0x21; // ATAPI-2 response format
            b[5..8].fill(0);
            padstr8(&mut b[8..16], "QEMU");
            padstr8(&mut b[16..32], CD_MODEL);
            padstr8(&mut b[32..36], FIRMWARE_VERSION);
            idx = 36;
        }
        b[size_idx] = (idx - preamble_len) as u8;
        self.atapi_cmd_reply(bus, idx, max_len);
    }

    fn cmd_start_stop_unit(&mut self, bus: &mut Bus) {
        let v = self.io_buffer[4];
        let start = v & 1 != 0;
        let loej = v & 2 != 0;
        if v & 0xf0 == 0 && loej {
            if !start && !self.tray_open && self.tray_locked {
                let sense = if self.present() { NOT_READY } else { ILLEGAL_REQUEST };
                self.atapi_cmd_error(bus, sense, ASC_MEDIA_REMOVAL_PREVENTED);
                return;
            }
            self.tray_open = !start;
        }
        self.atapi_cmd_ok(bus);
    }

    fn cmd_read_capacity(&mut self, bus: &mut Bus) {
        let last = self.total_cd_sectors().wrapping_sub(1) as u32;
        self.io_buffer[0..4].copy_from_slice(&last.to_be_bytes());
        self.io_buffer[4..8].copy_from_slice(&2048u32.to_be_bytes());
        self.atapi_cmd_reply(bus, 8, 8);
    }

    /// READ(10) and READ(12).
    fn cmd_read(&mut self, bus: &mut Bus) {
        let b = &self.io_buffer;
        let nb_sectors = if b[0] == 0x28 {
            be16(&b[7..]) as u32
        } else {
            be32(&b[6..])
        };
        if nb_sectors == 0 {
            self.atapi_cmd_ok(bus);
            return;
        }
        let lba = be32(&b[2..]);
        let total = self.total_cd_sectors();
        if lba as u64 >= total || lba.wrapping_add(nb_sectors - 1) as u64 >= total {
            self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_LOGICAL_BLOCK_OOR);
            return;
        }
        self.atapi_cmd_read(bus, lba as i64, nb_sectors, CD_SECTOR);
    }

    fn cmd_seek(&mut self, bus: &mut Bus) {
        let lba = be32(&self.io_buffer[2..]);
        if lba as u64 >= self.total_cd_sectors() {
            self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_LOGICAL_BLOCK_OOR);
            return;
        }
        self.atapi_cmd_ok(bus);
    }

    /// READ TOC/PMA/ATIP. As in QEMU, the format comes from the old SFF-8020
    /// field in the top two bits of byte 9.
    fn cmd_read_toc(&mut self, bus: &mut Bus) {
        let b = &self.io_buffer;
        let max_len = be16(&b[7..]) as usize;
        let format = b[9] >> 6;
        let msf = (b[1] >> 1) & 1 != 0;
        let start_track = b[6];
        let total = self.total_cd_sectors() as i64;
        let len = match format {
            0 => cdrom_read_toc(total, &mut self.io_buffer, msf, start_track),
            1 => {
                self.io_buffer[..12].fill(0);
                self.io_buffer[1] = 0x0a;
                self.io_buffer[2] = 0x01;
                self.io_buffer[3] = 0x01;
                Some(12)
            }
            2 => Some(cdrom_read_toc_raw(total, &mut self.io_buffer, msf)),
            _ => None,
        };
        match len {
            Some(len) => self.atapi_cmd_reply(bus, len, max_len),
            None => self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_INV_FIELD_IN_CMD_PACKET),
        }
    }

    fn cmd_get_configuration(&mut self, bus: &mut Bus) {
        if self.io_buffer[2] != 0 || self.io_buffer[3] != 0 {
            self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_INV_FIELD_IN_CMD_PACKET);
            return;
        }
        let max_len = (be16(&self.io_buffer[7..]) as usize).min(512);
        let current = if !self.media_present() {
            None
        } else if self.nb_sectors() > CD_MAX_SECTORS {
            Some(MMC_PROFILE_DVD_ROM)
        } else {
            Some(MMC_PROFILE_CD_ROM)
        };
        let b = &mut self.io_buffer;
        b[..max_len].fill(0);
        if let Some(p) = current {
            b[6..8].copy_from_slice(&p.to_be_bytes());
        }
        b[10] = 0x03; // persistent, current
        let mut len = 12;
        for (i, profile) in [MMC_PROFILE_DVD_ROM, MMC_PROFILE_CD_ROM].into_iter().enumerate() {
            let at = 12 + 4 * i;
            b[at..at + 2].copy_from_slice(&profile.to_be_bytes());
            b[at + 2] = (b[at] == b[6] && b[at + 1] == b[7]) as u8;
            b[11] = b[11].wrapping_add(4);
            len += 4;
        }
        b[..4].copy_from_slice(&(len as u32 - 4).to_be_bytes());
        self.atapi_cmd_reply(bus, len, max_len);
    }

    /// GET EVENT STATUS NOTIFICATION: polled mode, media class only. The
    /// medium is present from power-on, so no NEW MEDIA event is pending.
    fn cmd_get_event_status_notification(&mut self, bus: &mut Bus) {
        let b = &mut self.io_buffer;
        let max_len = be16(&b[7..]) as usize;
        if b[1] & 0x01 == 0 {
            self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_INV_FIELD_IN_CMD_PACKET);
            return;
        }
        const GESN_MEDIA: u8 = 4;
        let class = b[4];
        b[3] = 1 << GESN_MEDIA;
        b[2] = 0;
        let used_len = if class & (1 << GESN_MEDIA) != 0 {
            b[2] |= GESN_MEDIA;
            let media_status = if self.tray_open {
                1
            } else if self.media.is_some() {
                2
            } else {
                0
            };
            b[4] = 0; // no change
            b[5] = media_status;
            b[6] = 0;
            b[7] = 0;
            8
        } else {
            b[2] = 0x80; // no event available
            4
        };
        b[0..2].copy_from_slice(&(used_len as u16 - 4).to_be_bytes());
        self.atapi_cmd_reply(bus, used_len, max_len);
    }

    fn cmd_read_disc_information(&mut self, bus: &mut Bus) {
        let b = &mut self.io_buffer;
        let max_len = be16(&b[7..]) as usize;
        if b[1] & 7 != 0 {
            self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_INV_FIELD_IN_CMD_PACKET);
            return;
        }
        b[..34].fill(0);
        b[1] = 32;
        b[2] = 0x0e; // last session complete, disc finalized
        b[3] = 1;
        b[4] = 1;
        b[5] = 1;
        b[6] = 1;
        b[7] = 0x20; // unrestricted use
        self.atapi_cmd_reply(bus, 34, max_len);
    }

    /// MODE SENSE(10): current values of the error recovery (01h), audio
    /// control (0Eh) and capabilities (2Ah) pages. Like QEMU the audio page
    /// leaves bytes 10-16, 18, 20 and 22 as whatever the buffer held.
    fn cmd_mode_sense(&mut self, bus: &mut Bus) {
        let b = &mut self.io_buffer;
        let max_len = be16(&b[7..]) as usize;
        let action = b[2] >> 6;
        let code = b[2] & 0x3f;
        if action == 3 {
            self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_SAVING_PARAMETERS_NOT_SUPPORTED);
            return;
        }
        let len = match (action, code) {
            (0, 0x01) => {
                b[..8].copy_from_slice(&[0, 14, 0x70, 0, 0, 0, 0, 0]);
                b[8..16].copy_from_slice(&[0x01, 6, 0x00, 0x05, 0, 0, 0, 0]);
                16
            }
            (0, 0x0e) => {
                b[..8].copy_from_slice(&[0, 22, 0x70, 0, 0, 0, 0, 0]);
                b[8] = 0x0e;
                b[9] = 14;
                for i in [17, 19, 21, 23] {
                    b[i] = 0;
                }
                24
            }
            (0, 0x2a) => {
                b[..8].copy_from_slice(&[0, 28, 0x70, 0, 0, 0, 0, 0]);
                b[8] = 0x2a;
                b[9] = 20;
                b[10] = 0x3b;
                b[11] = 0x00;
                b[12] = 0x71;
                b[13] = 3 << 5;
                b[14] = (1 << 0) | (1 << 3) | (1 << 5) | if self.tray_locked { 1 << 1 } else { 0 };
                b[15] = 0x00;
                b[16..18].copy_from_slice(&704u16.to_be_bytes());
                b[18] = 0;
                b[19] = 2;
                b[20..22].copy_from_slice(&512u16.to_be_bytes());
                b[22..24].copy_from_slice(&704u16.to_be_bytes());
                b[24..30].fill(0);
                30
            }
            _ => {
                self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_INV_FIELD_IN_CMD_PACKET);
                return;
            }
        };
        self.atapi_cmd_reply(bus, len, max_len);
    }

    /// READ DVD STRUCTURE. The medium is a CD, so only the capability list
    /// (format FFh) is answered.
    fn cmd_read_dvd_structure(&mut self, bus: &mut Bus) {
        let b = &self.io_buffer;
        let media = b[1];
        let format = b[7];
        let max_len = be16(&b[8..]) as usize;
        let is_cd = self.media_present() && self.nb_sectors() <= CD_MAX_SECTORS;
        if format < 0xff {
            let asc = if is_cd { ASC_INCOMPATIBLE_FORMAT } else { ASC_INV_FIELD_IN_CMD_PACKET };
            if is_cd || !self.media_present() {
                self.atapi_cmd_error(bus, ILLEGAL_REQUEST, asc);
                return;
            }
            // Deliberate simplification: DVD-sized images are not supported.
            unhandled("atapi dvd structure", format as u64, 1, None);
            self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_INV_FIELD_IN_CMD_PACKET);
            return;
        }
        let b = &mut self.io_buffer;
        b[..max_len.min(IO_BUFFER_LEN)].fill(0);
        if media != 0 {
            self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_INV_FIELD_IN_CMD_PACKET);
            return;
        }
        b[4] = 0x00;
        b[5] = 0x40;
        b[6..8].copy_from_slice(&(2048u16 + 4).to_be_bytes());
        b[8] = 0x01;
        b[9] = 0x40;
        b[10..12].copy_from_slice(&(4u16 + 4).to_be_bytes());
        b[12] = 0x03;
        b[13] = 0x40;
        b[14..16].copy_from_slice(&(188u16 + 4).to_be_bytes());
        b[16] = 0x04;
        b[17] = 0x40;
        b[18..20].copy_from_slice(&(2048u16 + 4).to_be_bytes());
        b[0..2].copy_from_slice(&(16u16 + 2).to_be_bytes());
        self.atapi_cmd_reply(bus, 20, max_len);
    }

    fn cmd_mechanism_status(&mut self, bus: &mut Bus) {
        let b = &mut self.io_buffer;
        let max_len = be16(&b[8..]) as usize;
        b[..8].copy_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0]);
        self.atapi_cmd_reply(bus, 8, max_len);
    }

    /// READ CD: user data (2048) or full raw sectors (2352) only. As in QEMU
    /// the LBA is not range-checked up front; running off the end fails
    /// with LOGICAL BLOCK ADDRESS OUT OF RANGE when that sector is reached.
    fn cmd_read_cd(&mut self, bus: &mut Bus) {
        let b = &self.io_buffer;
        let nb_sectors = (b[6] as u32) << 16 | (b[7] as u32) << 8 | b[8] as u32;
        let lba = be32(&b[2..]) as i32 as i64;
        let transfer_request = b[9] & 0xf8;
        if nb_sectors == 0 || transfer_request == 0 {
            self.atapi_cmd_ok(bus);
            return;
        }
        if !self.validate_bcl() {
            return;
        }
        match transfer_request {
            0x10 => self.atapi_cmd_read(bus, lba, nb_sectors, 2048),
            0xf8 => self.atapi_cmd_read(bus, lba, nb_sectors, 2352),
            _ => self.atapi_cmd_error(bus, ILLEGAL_REQUEST, ASC_INV_FIELD_IN_CMD_PACKET),
        }
    }
}

/// QEMU's `atapi_cmd_table`: flags of each implemented packet command.
fn atapi_flags(op: u8) -> Option<u8> {
    Some(match op {
        0x00 => CHECK_READY | NONDATA,
        0x03 => ALLOW_UA,
        0x12 => ALLOW_UA,
        0x1b => NONDATA,
        0x1e => NONDATA,
        0x25 => CHECK_READY,
        0x28 => CHECK_READY,
        0x2b => CHECK_READY | NONDATA,
        0x43 => CHECK_READY,
        0x46 => ALLOW_UA,
        0x4a => ALLOW_UA,
        0x51 => CHECK_READY,
        0x5a => 0,
        0xa8 => CHECK_READY,
        0xad => CHECK_READY,
        0xbb => NONDATA,
        0xbd => 0,
        0xbe => CHECK_READY | CONDDATA,
        _ => return None,
    })
}

/// ATA commands in QEMU's command table (plus NOP). Aborting one of these
/// on a packet device or an empty slot is modelled behavior, not a gap.
fn qemu_known_ata_command(cmd: u8) -> bool {
    matches!(
        cmd,
        0x00 | 0x03 | 0x06 | 0x08 | 0x10 | 0x20 | 0x21 | 0x24 | 0x25 | 0x27 | 0x29
            | 0x30 | 0x31 | 0x34 | 0x35 | 0x38 | 0x39 | 0x3c | 0x40 | 0x41 | 0x42
            | 0x70 | 0x87 | 0x90 | 0x91 | 0x94..=0x99 | 0xa0 | 0xa1 | 0xb0 | 0xc0
            | 0xc4..=0xc6 | 0xc8..=0xcb | 0xcd | 0xe0..=0xe7 | 0xea | 0xec | 0xef
            | 0xf5 | 0xf8
    )
}

/// IDENTIFY PACKET DEVICE words, as QEMU's `ide_atapi_identify` builds them.
/// QEMU `ide_identify` for a hard disk (write cache on, no WWN, 512-byte
/// physical sectors, rotation rate unreported). Like QEMU 8.2's ide-hd it
/// advertises TRIM (words 69 and 169); the DATA SET MANAGEMENT command
/// itself isn't implemented (it aborts), and neither SeaBIOS nor TempleOS
/// issues it.
fn hd_identify(d: &Drive) -> [u16; 256] {
    let mut w = [0u16; 256];
    w[0] = 0x0040;
    w[1] = d.cylinders as u16;
    w[3] = d.heads as u16;
    w[4] = (512 * d.sectors) as u16;
    w[5] = 512;
    w[6] = d.sectors as u16;
    put_ata_string(&mut w[10..20], &d.serial);
    w[20] = 3;
    w[21] = 512;
    w[22] = 4;
    put_ata_string(&mut w[23..27], FIRMWARE_VERSION);
    put_ata_string(&mut w[27..47], HD_MODEL);
    w[47] = 0x8000 | u16::from(MAX_MULT_SECTORS);
    w[48] = 1;
    w[49] = (1 << 11) | (1 << 9) | (1 << 8);
    w[51] = 0x200;
    w[52] = 0x200;
    w[53] = 1 | (1 << 1) | (1 << 2);
    w[54] = d.cylinders as u16;
    w[55] = d.heads as u16;
    w[56] = d.sectors as u16;
    let oldsize = d.cylinders * d.heads * d.sectors;
    w[57] = oldsize as u16;
    w[58] = (oldsize >> 16) as u16;
    if d.mult_sectors != 0 {
        w[59] = 0x100 | u16::from(d.mult_sectors);
    }
    let nb = d.nb_sectors();
    let lba28 = nb.min((1 << 28) - 1);
    w[60] = lba28 as u16;
    w[61] = (lba28 >> 16) as u16;
    w[62] = 0x07;
    w[63] = 0x07;
    w[64] = 0x03;
    w[65] = 120;
    w[66] = 120;
    w[67] = 120;
    w[68] = 120;
    w[69] = 1 << 14; // determinate TRIM behavior
    w[80] = 0xf0;
    w[81] = 0x16;
    w[82] = (1 << 14) | (1 << 5) | 1;
    w[83] = (1 << 14) | (1 << 13) | (1 << 12) | (1 << 10);
    w[84] = 1 << 14;
    w[85] = (1 << 14) | (1 << 5) | 1;
    w[86] = (1 << 13) | (1 << 12) | (1 << 10);
    w[87] = 1 << 14;
    w[88] = 0x3f | (1 << 13);
    w[93] = 1 | (1 << 14) | 0x2000;
    w[100] = nb as u16;
    w[101] = (nb >> 16) as u16;
    w[102] = (nb >> 32) as u16;
    w[103] = (nb >> 48) as u16;
    w[106] = 0x6000;
    w[169] = 1; // TRIM supported
    w
}

fn atapi_identify(serial: &str) -> [u16; 256] {
    let mut w = [0u16; 256];
    // Removable CD-ROM, 50 us DRQ response, 12-byte packets.
    w[0] = (2 << 14) | (5 << 8) | (1 << 7) | (2 << 5);
    put_ata_string(&mut w[10..20], serial);
    w[20] = 3; // buffer type
    w[21] = 512; // cache size in sectors
    w[22] = 4; // ECC bytes
    put_ata_string(&mut w[23..27], FIRMWARE_VERSION);
    put_ata_string(&mut w[27..47], CD_MODEL);
    w[48] = 1; // dword I/O
    w[49] = (1 << 9) | (1 << 8); // LBA and DMA
    w[53] = 7; // words 54-58, 64-70 and 88 valid
    w[62] = 7; // single word DMA 0-2
    w[63] = 7; // multiword DMA 0-2
    w[64] = 3; // PIO 3-4
    w[65] = 0xb4;
    w[66] = 0xb4;
    w[67] = 0x12c;
    w[68] = 0xb4;
    w[71] = 30;
    w[72] = 30;
    w[80] = 0x1e; // ATA/ATAPI-1 to 4
    w[88] = 0x3f | (1 << 13); // UDMA 0-5 supported, UDMA 5 selected
    w
}

/// ATA string: space padded, two characters per word, first one high.
fn put_ata_string(words: &mut [u16], s: &str) {
    let bytes = s.as_bytes();
    let ch = |i: usize| *bytes.get(i).unwrap_or(&b' ') as u16;
    for (i, w) in words.iter_mut().enumerate() {
        *w = ch(2 * i) << 8 | ch(2 * i + 1);
    }
}

/// SCSI string: space padded, in order.
fn padstr8(buf: &mut [u8], s: &str) {
    let bytes = s.as_bytes();
    for (i, b) in buf.iter_mut().enumerate() {
        *b = *bytes.get(i).unwrap_or(&b' ');
    }
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn lba_to_msf(lba: i64) -> [u8; 3] {
    let l = lba + 150;
    [((l / 75) / 60) as u8, ((l / 75) % 60) as u8, (l % 75) as u8]
}

/// Writes an LBA or MSF address (4 bytes, MSF with a reserved lead byte).
fn put_address(q: &mut [u8], msf: bool, lba: i64) {
    if msf {
        q[0] = 0;
        q[1..4].copy_from_slice(&lba_to_msf(lba));
    } else {
        q[..4].copy_from_slice(&(lba as u32).to_be_bytes());
    }
}

/// READ TOC format 0: one data track plus the lead-out (QEMU's
/// `cdrom_read_toc`, itself taken from Bochs).
fn cdrom_read_toc(nb_sectors: i64, buf: &mut [u8], msf: bool, start_track: u8) -> Option<usize> {
    if start_track > 1 && start_track != 0xaa {
        return None;
    }
    buf[2] = 1; // first track
    buf[3] = 1; // last track
    let mut q = 4;
    if start_track <= 1 {
        buf[q..q + 4].copy_from_slice(&[0, 0x14, 1, 0]);
        put_address(&mut buf[q + 4..], msf, 0);
        q += 8;
    }
    buf[q..q + 4].copy_from_slice(&[0, 0x16, 0xaa, 0]);
    put_address(&mut buf[q + 4..], msf, nb_sectors);
    q += 8;
    buf[0..2].copy_from_slice(&((q - 2) as u16).to_be_bytes());
    Some(q)
}

/// READ TOC format 2 (raw TOC): A0/A1/A2 points and track 1.
fn cdrom_read_toc_raw(nb_sectors: i64, buf: &mut [u8], msf: bool) -> usize {
    buf[2] = 1; // first session
    buf[3] = 1; // last session
    buf[4..15].copy_from_slice(&[1, 0x14, 0, 0xa0, 0, 0, 0, 0, 1, 0x00, 0x00]);
    buf[15..26].copy_from_slice(&[1, 0x14, 0, 0xa1, 0, 0, 0, 0, 1, 0x00, 0x00]);
    buf[26..33].copy_from_slice(&[1, 0x14, 0, 0xa2, 0, 0, 0]);
    put_address(&mut buf[33..], msf, nb_sectors);
    buf[37..44].copy_from_slice(&[1, 0x14, 0, 1, 0, 0, 0]);
    if msf {
        put_address(&mut buf[44..], true, 0);
    } else {
        buf[44..48].fill(0);
    }
    let len = 48;
    buf[0..2].copy_from_slice(&((len - 2) as u16).to_be_bytes());
    len
}

/// One IDE channel (two drive slots) in legacy compatibility mode: command
/// block (base 0x1F0/0x170, registers 0-7) and control block (0x3F6/0x376).
/// PIO only: no DMA, no bus master. IRQ 14/15 are masked by TempleOS, but
/// the INTRQ level is kept exact because the PIC's IRR is guest visible.
///
/// Register semantics follow QEMU's `ide_ioport_read/write`:
/// - Writes to registers 1-6 go to both drives; each keeps the previous
///   value as its HOB byte. Any command-block write clears HOB in the device
///   control register. Writes to registers 1-6 are dropped while the
///   selected drive has BSY or DRQ set.
/// - With no drive on the channel every register reads 0. With only a
///   master, the absent slave's status and error read 0 but its other
///   registers read back normally.
/// - Reading status (register 7) lowers INTRQ; the alternate status does
///   not. Writing a command lowers INTRQ before the command runs.
pub struct IdeChannel {
    drives: [Drive; 2],
    unit: usize,
    bus: Bus,
}

impl IdeChannel {
    /// True if the master slot holds a CD.
    pub fn has_cdrom(&self) -> bool {
        self.drives[0].is_cd()
    }

    /// The disk geometry of drive `unit`, for the CMOS setup QEMU's
    /// `pc_cmos_init` does. None if the slot holds no hard disk.
    pub fn disk_geometry(&self, unit: usize) -> Option<Geometry> {
        let d = &self.drives[unit];
        d.is_disk().then_some(Geometry {
            cylinders: d.cylinders,
            heads: d.drive_heads,
            sectors: d.drive_sectors,
            translation: d.translation,
        })
    }

    /// A channel wired as QEMU's secondary channel (the reference machine's
    /// CD slot): drive serials are QM00003 and QM00004. Use
    /// [`IdeChannel::new_on_channel`] for the primary.
    pub fn new(master: Option<Media>, slave: Option<Media>) -> Self {
        Self::new_on_channel(1, master, slave)
    }

    /// A channel with QEMU's drive numbering for channel `index` (0 =
    /// primary, 1 = secondary): serial numbers are `QM0000n` with n =
    /// 2 * index + unit + 1.
    pub fn new_on_channel(index: usize, master: Option<Media>, slave: Option<Media>) -> Self {
        IdeChannel {
            drives: [Drive::new(master, 2 * index + 1), Drive::new(slave, 2 * index + 2)],
            unit: 0,
            bus: Bus { devctl: 0, irq: false },
        }
    }

    fn no_drives(&self) -> bool {
        !self.drives[0].present() && !self.drives[1].present()
    }

    /// Whether the selected slot hides its status (no drives, or an empty
    /// slave slot).
    fn status_hidden(&self) -> bool {
        self.no_drives() || (self.unit == 1 && !self.drives[1].present())
    }

    /// Command block read. `reg` is 0-7; the data register (0) takes 16- and
    /// 32-bit accesses, the rest are byte registers.
    pub fn read(&mut self, reg: u8, size: u8) -> u32 {
        let reg = reg & 7;
        if reg == 0 {
            let s = &mut self.drives[self.unit];
            return match size {
                2 | 4 => s.data_read(&mut self.bus, size as usize),
                // QEMU routes byte accesses to the task file handler, which
                // returns 0xff for the data register.
                _ => 0xff,
            };
        }
        if size != 1 {
            unhandled("ide wide read", reg as u64, size, None);
        }
        let hob = self.bus.devctl & CTRL_HOB != 0;
        let empty = self.no_drives();
        let hidden = self.status_hidden();
        let s = &self.drives[self.unit];
        let pick = |low: u8, high: u8| {
            if empty {
                0
            } else if hob {
                high
            } else {
                low
            }
        };
        let v = match reg {
            1 if hidden => 0,
            1 => pick(s.error, s.hob_feature),
            2 => pick(s.nsector as u8, s.hob_nsector),
            3 => pick(s.sector, s.hob_sector),
            4 => pick(s.lcyl, s.hob_lcyl),
            5 => pick(s.hcyl, s.hob_hcyl),
            6 => pick(s.select, s.select),
            _ => {
                let v = if hidden { 0 } else { s.status };
                self.bus.irq = false;
                v
            }
        };
        v as u32
    }

    /// Command block write. See [`IdeChannel::read`] for sizes.
    pub fn write(&mut self, reg: u8, size: u8, val: u32) {
        let reg = reg & 7;
        if reg == 0 {
            match size {
                2 | 4 => self.drives[self.unit].data_write(&mut self.bus, size as usize, val),
                _ => {} // byte writes to the data register are dropped, as in QEMU
            }
            return;
        }
        if size != 1 {
            unhandled("ide wide write", reg as u64, size, Some(val as u64));
        }
        let val = val as u8;
        if reg != 7 && self.drives[self.unit].status & (BUSY_STAT | DRQ_STAT) != 0 {
            return;
        }
        self.bus.devctl &= !CTRL_HOB;
        match reg {
            1 => self.shift_in(val, |d| (&mut d.feature, &mut d.hob_feature)),
            2 => {
                // QEMU keeps the (possibly 16-bit) count and truncates it
                // into the HOB byte.
                for d in &mut self.drives {
                    d.hob_nsector = d.nsector as u8;
                    d.nsector = u32::from(val);
                }
            }
            3 => self.shift_in(val, |d| (&mut d.sector, &mut d.hob_sector)),
            4 => self.shift_in(val, |d| (&mut d.lcyl, &mut d.hob_lcyl)),
            5 => self.shift_in(val, |d| (&mut d.hcyl, &mut d.hob_hcyl)),
            6 => {
                for d in &mut self.drives {
                    d.select = val | DEV_ALWAYS_ON;
                }
                self.unit = usize::from(val & DEV_SELECT != 0);
            }
            _ => {
                self.bus.irq = false;
                // Commands to an absent slave are ignored.
                if self.unit == 1 && !self.drives[1].present() {
                    return;
                }
                self.drives[self.unit].ata_command(&mut self.bus, val);
            }
        }
    }

    /// Writes a task file register in both drives, keeping the old value as
    /// the HOB byte (for LBA48).
    fn shift_in(&mut self, val: u8, reg: impl Fn(&mut Drive) -> (&mut u8, &mut u8)) {
        for d in &mut self.drives {
            let (cur, hob) = reg(d);
            *hob = *cur;
            *cur = val;
        }
    }

    /// Alternate status (control block read): the status register without
    /// the side effect of lowering INTRQ.
    pub fn read_alt_status(&mut self) -> u8 {
        if self.status_hidden() {
            0
        } else {
            self.drives[self.unit].status
        }
    }

    /// Device control register: nIEN (bit 1), SRST (bit 2), HOB (bit 7).
    ///
    /// Setting SRST resets both drives and runs their diagnostics: the CD
    /// comes back with status 0 and its signature. QEMU does this in a
    /// bottom half (the drives read BSY until it runs); here it happens at
    /// once. As in QEMU the SRST bit then drops out of the stored register,
    /// so every write with SRST set is a new reset, and the drive selection
    /// is left as it was.
    pub fn write_device_control(&mut self, val: u8) {
        let rising = self.bus.devctl & CTRL_SRST == 0 && val & CTRL_SRST != 0;
        self.bus.devctl = val;
        if rising {
            for d in &mut self.drives {
                d.transfer_halt();
                d.reset();
                d.cmd_exec_dev_diagnostic(&mut self.bus);
            }
            self.bus.devctl &= !CTRL_SRST;
        }
    }

    /// Level of the channel's INTRQ line (IRQ 14 or 15).
    pub fn irq_level(&self) -> bool {
        self.bus.irq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: u8 = 0;
    const FEATURE: u8 = 1;
    const NSECT: u8 = 2;
    const LBA_LOW: u8 = 3;
    const LBA_MID: u8 = 4;
    const LBA_HIGH: u8 = 5;
    const DEVICE: u8 = 6;
    const STATUS: u8 = 7;

    fn workspace_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn load_iso() -> Option<&'static [u8]> {
        let data = std::fs::read(workspace_root().join("payload/TempleOS.ISO")).ok()?;
        Some(Box::leak(data.into_boxed_slice()))
    }

    /// A 64-block image whose every block starts with its block number, with
    /// a primary volume descriptor at block 16.
    fn synthetic_image() -> &'static [u8] {
        let mut img = vec![0u8; 64 * CD_SECTOR];
        for (i, blk) in img.chunks_mut(CD_SECTOR).enumerate() {
            blk[..4].copy_from_slice(&(i as u32).to_be_bytes());
            blk[4] = 0xa5;
        }
        img[16 * CD_SECTOR..16 * CD_SECTOR + 6].copy_from_slice(b"\x01CD001");
        Box::leak(img.into_boxed_slice())
    }

    fn cd_channel() -> IdeChannel {
        IdeChannel::new(Some(Media::Cdrom(synthetic_image())), None)
    }

    /// Sends a packet command with the given byte count limit; leaves the
    /// channel in whatever phase the command produced.
    fn send_packet(ch: &mut IdeChannel, packet: &[u8], bcl: u16) {
        ch.write(DEVICE, 1, 0xa0);
        ch.write(FEATURE, 1, 0);
        ch.write(LBA_MID, 1, (bcl & 0xff) as u32);
        ch.write(LBA_HIGH, 1, (bcl >> 8) as u32);
        ch.write(STATUS, 1, 0xa0);
        assert_eq!(ch.read(STATUS, 1), 0x58, "packet phase");
        assert_eq!(ch.read(NSECT, 1), 0x01, "CoD=1 IO=0");
        let mut p = [0u8; 12];
        p[..packet.len()].copy_from_slice(packet);
        for w in p.chunks(2) {
            ch.write(DATA, 2, u16::from_le_bytes([w[0], w[1]]) as u32);
        }
    }

    /// Reads every data-in block of the current command. Returns the data
    /// and the size of each DRQ block (from LBA mid/high).
    fn read_all(ch: &mut IdeChannel) -> (Vec<u8>, Vec<usize>) {
        let mut data = Vec::new();
        let mut blocks = Vec::new();
        while ch.read_alt_status() & DRQ_STAT != 0 {
            assert_eq!(ch.read(NSECT, 1) & 3, INT_REASON_IO as u32, "data phase");
            let n = ch.read(LBA_MID, 1) as usize | (ch.read(LBA_HIGH, 1) as usize) << 8;
            blocks.push(n);
            let mut left = n;
            while left > 0 && ch.read_alt_status() & DRQ_STAT != 0 {
                let w = ch.read(DATA, 2) as u16;
                data.extend_from_slice(&w.to_le_bytes());
                left = left.saturating_sub(2);
            }
        }
        (data, blocks)
    }

    fn packet_in(ch: &mut IdeChannel, packet: &[u8], bcl: u16) -> Vec<u8> {
        send_packet(ch, packet, bcl);
        let (data, _) = read_all(ch);
        assert_eq!(ch.read(STATUS, 1), 0x50, "command completes cleanly");
        assert_eq!(ch.read(NSECT, 1) & 3, 3, "CoD=1 IO=1");
        data
    }

    #[test]
    fn signature_after_srst() {
        let mut ch = cd_channel();
        assert_eq!(ch.read(STATUS, 1), 0x50, "power-on status");
        ch.write(NSECT, 1, 0x55);
        ch.write_device_control(0x0e);
        ch.write_device_control(0x0a);
        assert_eq!(ch.read(STATUS, 1), 0x00, "ATAPI diagnostic leaves status clear");
        assert_eq!(ch.read(FEATURE, 1), 0x01, "diagnostic code");
        assert_eq!(ch.read(NSECT, 1), 0x01);
        assert_eq!(ch.read(LBA_LOW, 1), 0x01);
        assert_eq!(ch.read(LBA_MID, 1), 0x14);
        assert_eq!(ch.read(LBA_HIGH, 1), 0xeb);
        assert_eq!(ch.read(DEVICE, 1), 0xa0);
        assert!(!ch.irq_level(), "nIEN was set");

        // The absent slave hides its status but not its task file.
        ch.write(DEVICE, 1, 0xb0);
        assert_eq!(ch.read(STATUS, 1), 0);
        assert_eq!(ch.read(FEATURE, 1), 0);
        assert_eq!(ch.read(LBA_MID, 1), 0xff);
        assert_eq!(ch.read(DEVICE, 1), 0xb0);
    }

    #[test]
    fn identify_device_aborts_with_signature() {
        let mut ch = cd_channel();
        ch.write(LBA_MID, 1, 0);
        ch.write(STATUS, 1, 0xec);
        assert_eq!(ch.read_alt_status(), 0x41);
        assert_eq!(ch.read(FEATURE, 1), ABRT_ERR as u32);
        assert_eq!(ch.read(LBA_MID, 1), 0x14);
        assert_eq!(ch.read(LBA_HIGH, 1), 0xeb);
        assert!(ch.irq_level());
    }

    #[test]
    fn identify_packet_matches_qemu() {
        let fixture = std::fs::read_to_string(workspace_root().join("docs/ref/atapi-identify.txt"))
            .expect("docs/ref/atapi-identify.txt");
        let expect: Vec<u32> = fixture
            .lines()
            .filter(|l| !l.starts_with('#'))
            .flat_map(|l| l.split_whitespace())
            .map(|w| u32::from_str_radix(w, 16).unwrap())
            .collect();
        assert_eq!(expect.len(), 256);

        let mut ch = cd_channel();
        ch.write(STATUS, 1, 0xa1);
        assert_eq!(ch.read_alt_status(), 0x58);
        assert!(ch.irq_level());
        let got: Vec<u32> = (0..256).map(|_| ch.read(DATA, 2)).collect();
        assert_eq!(got, expect);
        assert_eq!(ch.read(STATUS, 1), 0x50);
        assert!(!ch.irq_level(), "no interrupt after the last data word");
    }

    #[test]
    fn read12_block16_is_primary_volume_descriptor() {
        let (img, real) = match load_iso() {
            Some(img) => (img, true),
            None => {
                eprintln!("payload/TempleOS.ISO missing; using a synthetic image");
                (synthetic_image(), false)
            }
        };
        let mut ch = IdeChannel::new(Some(Media::Cdrom(img)), None);
        let data = packet_in(&mut ch, &[0xa8, 0, 0, 0, 0, 16, 0, 0, 0, 1], 0x800);
        assert_eq!(data.len(), 2048);
        assert_eq!(&data[..6], b"\x01CD001");
        assert_eq!(data, img[16 * 2048..17 * 2048]);
        if real {
            assert_eq!(data[6], 1, "descriptor version");
        }
    }

    #[test]
    fn byte_count_chunking() {
        let mut ch = cd_channel();
        // Two sectors with a limit of 0x301 (odd): blocks of 0x300 bytes.
        send_packet(&mut ch, &[0x28, 0, 0, 0, 0, 3, 0, 0, 2], 0x301);
        let (data, blocks) = read_all(&mut ch);
        assert_eq!(blocks, vec![0x300, 0x300, 0x300, 0x300, 0x300, 0x100]);
        assert_eq!(&data[..5], &[0, 0, 0, 3, 0xa5]);
        assert_eq!(&data[2048..2053], &[0, 0, 0, 4, 0xa5]);
        assert_eq!(ch.read(STATUS, 1), 0x50);

        // A limit above one sector gives one DRQ block spanning both.
        send_packet(&mut ch, &[0x28, 0, 0, 0, 0, 3, 0, 0, 2], 0xffff);
        let (data, blocks) = read_all(&mut ch);
        assert_eq!(blocks, vec![4096]);
        assert_eq!(&data[2048..2052], &[0, 0, 0, 4]);

        // Zero limit on a data command aborts at the ATA level, no IRQ.
        ch.read(STATUS, 1);
        send_packet(&mut ch, &[0x28, 0, 0, 0, 0, 3, 0, 0, 1], 0);
        assert_eq!(ch.read_alt_status(), 0x41);
        assert_eq!(ch.read(FEATURE, 1), ABRT_ERR as u32);
        assert!(!ch.irq_level());

        // Replies shorter than the limit go in one block; allocation
        // length truncates them.
        let inq = packet_in(&mut ch, &[0x12, 0, 0, 0, 36], 0x800);
        assert_eq!(&inq[..8], &[0x05, 0x80, 0x00, 0x21, 31, 0, 0, 0]);
        assert_eq!(&inq[8..36], b"QEMU    QEMU DVD-ROM    2.5+");
        let cap = packet_in(&mut ch, &[0x25], 0x800);
        assert_eq!(cap, vec![0, 0, 0, 63, 0, 0, 8, 0]);
    }

    #[test]
    fn request_sense_after_invalid_opcode() {
        let mut ch = cd_channel();
        send_packet(&mut ch, &[0x52], 0x800);
        assert_eq!(ch.read_alt_status(), 0x41);
        assert_eq!(ch.read(FEATURE, 1), (ILLEGAL_REQUEST << 4) as u32);
        assert_eq!(ch.read(NSECT, 1) & 3, 3);
        assert!(ch.irq_level());
        ch.read(STATUS, 1);
        let sense = packet_in(&mut ch, &[0x03, 0, 0, 0, 18], 0x800);
        assert_eq!(sense.len(), 18);
        assert_eq!(sense[0], 0xf0);
        assert_eq!(sense[2], ILLEGAL_REQUEST);
        assert_eq!(sense[7], 10);
        assert_eq!(sense[12], ASC_ILLEGAL_OPCODE);

        // Out-of-range READ.
        send_packet(&mut ch, &[0x28, 0, 0, 0, 0, 63, 0, 0, 2], 0x800);
        assert_eq!(ch.read_alt_status(), 0x41);
        let sense = packet_in(&mut ch, &[0x03, 0, 0, 0, 18], 0x800);
        assert_eq!(sense[12], ASC_LOGICAL_BLOCK_OOR);
    }

    #[test]
    fn empty_channel_reads_zero() {
        let mut ch = IdeChannel::new_on_channel(0, None, None);
        for reg in 1..=7 {
            assert_eq!(ch.read(reg, 1), 0, "reg {reg}");
        }
        ch.write(NSECT, 1, 0x55);
        ch.write(DEVICE, 1, 0xe0);
        assert_eq!(ch.read(NSECT, 1), 0);
        assert_eq!(ch.read(DEVICE, 1), 0);
        assert_eq!(ch.read_alt_status(), 0);
        assert_eq!(ch.read(DATA, 2), 0);
        // IDENTIFY to the empty master still pulses INTRQ, as in QEMU.
        ch.write(STATUS, 1, 0xec);
        assert!(ch.irq_level());
        assert_eq!(ch.read(STATUS, 1), 0);
        assert!(!ch.irq_level());
    }

    #[test]
    fn irq_raise_and_clear() {
        let mut ch = cd_channel();
        assert!(!ch.irq_level());
        send_packet(&mut ch, &[0x00], 0);
        assert!(ch.irq_level(), "TEST UNIT READY completes with INTRQ");
        assert_eq!(ch.read_alt_status(), 0x50);
        assert!(ch.irq_level(), "alt status leaves INTRQ alone");
        assert_eq!(ch.read(STATUS, 1), 0x50);
        assert!(!ch.irq_level(), "status read clears INTRQ");

        // nIEN suppresses new assertions.
        ch.write_device_control(0x0a);
        send_packet(&mut ch, &[0x00], 0);
        assert!(!ch.irq_level());
        ch.write_device_control(0x08);

        // Each data block and the completion raise INTRQ.
        send_packet(&mut ch, &[0x28, 0, 0, 0, 0, 1, 0, 0, 1], 0x800);
        assert!(ch.irq_level());
        ch.read(STATUS, 1);
        for _ in 0..1024 {
            ch.read(DATA, 2);
        }
        assert!(ch.irq_level());
        assert_eq!(ch.read(STATUS, 1), 0x50);

        // A command write lowers INTRQ first.
        send_packet(&mut ch, &[0x00], 0);
        assert!(ch.irq_level());
        ch.write(STATUS, 1, 0xa0);
        assert!(!ch.irq_level());
    }

    #[test]
    fn register_writes_ignored_while_drq() {
        let mut ch = cd_channel();
        ch.write(STATUS, 1, 0xa0);
        ch.write(DEVICE, 1, 0xb0);
        assert_eq!(ch.read(DEVICE, 1), 0xa0, "selection change dropped");
        ch.write(NSECT, 1, 0x77);
        assert_eq!(ch.read(NSECT, 1), 0x01, "task file write dropped");
    }

    #[test]
    fn hob_reads_previous_bytes() {
        let mut ch = cd_channel();
        ch.write(LBA_LOW, 1, 0x12);
        ch.write(LBA_LOW, 1, 0x34);
        assert_eq!(ch.read(LBA_LOW, 1), 0x34);
        ch.write_device_control(0x80);
        assert_eq!(ch.read(LBA_LOW, 1), 0x12);
        ch.write(NSECT, 1, 0);
        assert_eq!(ch.read(LBA_LOW, 1), 0x34, "any write clears HOB");
    }

    #[test]
    fn misc_packet_commands() {
        let mut ch = cd_channel();
        let toc = packet_in(&mut ch, &[0x43, 0, 0, 0, 0, 0, 0, 0, 0xff, 0], 0xfffe);
        assert_eq!(toc, vec![0, 18, 1, 1, 0, 0x14, 1, 0, 0, 0, 0, 0, 0, 0x16, 0xaa, 0, 0, 0, 0, 64]);
        let toc1 = packet_in(&mut ch, &[0x43, 0, 0, 0, 0, 0, 0, 0, 0xff, 0x40], 0xfffe);
        assert_eq!(toc1, vec![0, 10, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
        let raw = packet_in(&mut ch, &[0x43, 0, 0, 0, 0, 0, 0, 0, 0xff, 0x80], 0xfffe);
        assert_eq!(raw.len(), 48);
        let conf = packet_in(&mut ch, &[0x46, 0, 0, 0, 0, 0, 0, 0, 32], 0xfffe);
        assert_eq!(conf, vec![0, 0, 0, 16, 0, 0, 0, 8, 0, 0, 3, 8, 0, 0x10, 0, 0, 0, 0x08, 1, 0]);
        let gesn = packet_in(&mut ch, &[0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8], 0xfffe);
        assert_eq!(gesn, vec![0, 4, 4, 0x10, 0, 2, 0, 0]);
        let caps = packet_in(&mut ch, &[0x5a, 0, 0x2a, 0, 0, 0, 0, 0, 30], 0xfffe);
        assert_eq!(&caps[..12], &[0, 28, 0x70, 0, 0, 0, 0, 0, 0x2a, 20, 0x3b, 0]);
        send_packet(&mut ch, &[0x5a, 0, 0xea], 0xfffe);
        assert_eq!(ch.read_alt_status(), 0x41);
        let mech = packet_in(&mut ch, &[0xbd, 0, 0, 0, 0, 0, 0, 0, 0, 8], 0xfffe);
        assert_eq!(mech, vec![0, 0, 0, 0, 0, 1, 0, 0]);
        let raw_sector = packet_in(&mut ch, &[0xbe, 0, 0, 0, 0, 2, 0, 0, 1, 0xf8], 0xfffe);
        assert_eq!(raw_sector.len(), 2352);
        assert_eq!(&raw_sector[..16], &[0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 0, 0, 2, 2, 1]);
        assert_eq!(&raw_sector[16..20], &[0, 0, 0, 2]);
        packet_in(&mut ch, &[0x2b, 0, 0, 0, 0, 5], 0);
        packet_in(&mut ch, &[0xbb], 0);
        packet_in(&mut ch, &[0x1e, 0, 0, 0, 1], 0);
        send_packet(&mut ch, &[0x1b, 0, 0, 0, 2], 0);
        assert_eq!(ch.read_alt_status(), 0x41, "eject prevented");
        assert_eq!(ch.read(FEATURE, 1), (NOT_READY << 4) as u32);
    }

    /// Replays QEMU's IDE port trace against the model: every write is
    /// applied, every read must return the traced value.
    ///
    /// Tolerance: QEMU reads CD sectors and performs SRST asynchronously, so
    /// the trace contains status/alt-status reads showing BSY while QEMU
    /// was still busy. The model finishes that work immediately. A traced
    /// status read with BSY set is skipped (not applied, so INTRQ is not
    /// lowered either) only when the model reports not busy at that point;
    /// every other read is compared exactly.
    #[test]
    #[ignore]
    fn replay_reference_trace() {
        use std::io::BufRead;

        let env_path = |name: &str| std::env::var_os(name).map(|p| workspace_root().join(p));
        let path = env_path("TEMPLEOS_TRACE").unwrap_or_else(|| workspace_root().join("ref/boot/trace.log"));
        let Ok(file) = std::fs::File::open(&path) else {
            eprintln!("skipping: trace {} not found", path.display());
            return;
        };
        // TEMPLEOS_CD=none: the run had no CD. TEMPLEOS_HDD: raw image the
        // run's primary master started from; TEMPLEOS_HDD_FINAL: the image
        // QEMU left, compared with the model's at the end.
        let cd = if std::env::var("TEMPLEOS_CD").is_ok_and(|v| v == "none") {
            None
        } else {
            let Some(iso) = load_iso() else {
                eprintln!("skipping: payload/TempleOS.ISO not found");
                return;
            };
            Some(Media::Cdrom(iso))
        };
        let hdd_image = env_path("TEMPLEOS_HDD").map(|p| {
            let img = std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            std::sync::Arc::new(std::sync::Mutex::new(img))
        });
        let hdd = hdd_image.clone().map(|img| Media::Disk(Box::new(SharedDisk(img)) as Box<dyn DiskImage>));
        let mut chans = [IdeChannel::new_on_channel(0, hdd, None), IdeChannel::new_on_channel(1, cd, None)];

        let (mut reads, mut matched, mut skipped_busy, mut writes) = (0u64, 0u64, 0u64, 0u64);
        let mut mismatches = Vec::new();
        for (lineno, line) in std::io::BufReader::new(file).lines().enumerate() {
            let line = line.unwrap();
            if !line.ends_with("name 'ide'") {
                continue;
            }
            let t: Vec<&str> = line.split_whitespace().collect();
            let is_read = match t[0] {
                "memory_region_ops_read" => true,
                "memory_region_ops_write" => false,
                _ => continue,
            };
            let num = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
            let addr = num(t[6]);
            let value = num(t[8]) as u32;
            let size = num(t[10]) as u8;
            let (ch, reg) = match addr {
                0x1f0..=0x1f7 => (0, Some((addr - 0x1f0) as u8)),
                0x3f6 => (0, None),
                0x170..=0x177 => (1, Some((addr - 0x170) as u8)),
                0x376 => (1, None),
                _ => panic!("line {}: unexpected IDE address {addr:#x}", lineno + 1),
            };
            let c = &mut chans[ch];
            if !is_read {
                writes += 1;
                match reg {
                    Some(r) => c.write(r, size, value),
                    None => c.write_device_control(value as u8),
                }
                continue;
            }
            reads += 1;
            let is_status = reg.is_none() || reg == Some(7);
            if is_status && value & BUSY_STAT as u32 != 0 && c.read_alt_status() & BUSY_STAT == 0 {
                skipped_busy += 1;
                continue;
            }
            let got = match reg {
                Some(r) => c.read(r, size),
                None => c.read_alt_status() as u32,
            };
            if got == value {
                matched += 1;
            } else {
                mismatches.push(format!(
                    "line {}: read {addr:#x} size {size}: trace {value:#x}, model {got:#x}",
                    lineno + 1
                ));
            }
        }
        eprintln!(
            "replay: {writes} writes, {reads} reads: {matched} matched, \
             {skipped_busy} BSY polls skipped, {} mismatches",
            mismatches.len()
        );
        for m in mismatches.iter().take(40) {
            eprintln!("  {m}");
        }
        assert!(reads > 0, "no IDE accesses in the trace");
        assert!(mismatches.is_empty(), "{} mismatching reads", mismatches.len());
        if let (Some(img), Some(final_path)) = (hdd_image, env_path("TEMPLEOS_HDD_FINAL")) {
            let want = std::fs::read(&final_path).unwrap();
            let got = img.lock().unwrap();
            let diff = got.iter().zip(&want).filter(|(a, b)| a != b).count();
            assert_eq!(got.len(), want.len());
            assert_eq!(diff, 0, "model disk differs from {} in {diff} bytes", final_path.display());
            eprintln!("disk image matches {}", final_path.display());
        }
    }

    /// A MemDisk the test can still look at after handing it to a drive.
    struct SharedDisk(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl DiskImage for SharedDisk {
        fn sectors(&self) -> u64 {
            self.0.lock().unwrap().len() as u64 / 512
        }
        fn read(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
            let img = self.0.lock().unwrap();
            let at = lba as usize * 512;
            buf.copy_from_slice(img.get(at..at + buf.len()).ok_or(io::ErrorKind::UnexpectedEof)?);
            Ok(())
        }
        fn write(&mut self, lba: u64, buf: &[u8]) -> io::Result<()> {
            let mut img = self.0.lock().unwrap();
            let at = lba as usize * 512;
            img.get_mut(at..at + buf.len()).ok_or(io::ErrorKind::UnexpectedEof)?.copy_from_slice(buf);
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
