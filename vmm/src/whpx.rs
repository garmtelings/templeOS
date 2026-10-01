//! Thin safe layer over the Windows Hypervisor Platform API: partitions,
//! guest-physical mappings, virtual processors and their registers.

use std::fmt;

use windows::Win32::System::Hypervisor::*;

pub use windows::Win32::System::Hypervisor::{
    WHV_REGISTER_NAME, WHV_REGISTER_VALUE, WHV_RUN_VP_EXIT_CONTEXT,
};

#[derive(Debug)]
pub struct Error {
    what: &'static str,
    source: windows::core::Error,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.what, self.source)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

trait Context<T> {
    fn context(self, what: &'static str) -> Result<T>;
}

impl<T> Context<T> for windows::core::Result<T> {
    fn context(self, what: &'static str) -> Result<T> {
        self.map_err(|source| Error { what, source })
    }
}

/// Why WHPX can't be used on this machine, with what to do about it; `Ok`
/// when it can. The exe delay-loads the WHPX DLLs, so this must run (and
/// succeed) before any other WHv call: calling into a missing delay-loaded
/// DLL crashes.
pub fn check_available() -> std::result::Result<(), String> {
    use windows::core::w;
    use windows::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    // SAFETY: loading system DLLs by name; they stay loaded for the process.
    let dlls = unsafe {
        LoadLibraryExW(w!("WinHvPlatform.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).is_ok()
            && LoadLibraryExW(w!("WinHvEmulation.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).is_ok()
    };
    if !dlls {
        return Err("TempleOS needs the Windows Hypervisor Platform, which is not installed.\n\n\
                    Turn it on in an administrator PowerShell:\n    \
                    Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform\n\
                    (or: Turn Windows features on or off > Windows Hypervisor Platform), then restart Windows."
            .into());
    }
    let mut present = 0u32;
    // SAFETY: the buffer is a u32, which is what this capability returns.
    let r = unsafe {
        WHvGetCapability(
            WHvCapabilityCodeHypervisorPresent,
            &mut present as *mut u32 as _,
            4,
            None,
        )
    };
    if r.is_err() || present == 0 {
        // The DLLs ship with Windows even when the feature is off, so this
        // can't tell "feature off" from "hypervisor told not to start".
        return Err("The Windows hypervisor is not running, so TempleOS can't start.\n\n\
                    Fix (once, in an administrator PowerShell), then restart Windows:\n    \
                    Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform\n    \
                    bcdedit /set hypervisorlaunchtype auto\n\n\
                    If that doesn't help, turn on virtualization (Intel VT-x / AMD-V, often \
                    called SVM) in the BIOS/UEFI settings."
            .into());
    }
    Ok(())
}

/// Guest access rights for a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ReadWriteExecute,
    ReadWrite,
    ReadExecute,
}

impl Access {
    fn flags(self) -> WHV_MAP_GPA_RANGE_FLAGS {
        match self {
            Access::ReadWriteExecute => WHV_MAP_GPA_RANGE_FLAGS(
                WHvMapGpaRangeFlagRead.0 | WHvMapGpaRangeFlagWrite.0 | WHvMapGpaRangeFlagExecute.0,
            ),
            Access::ReadWrite => {
                WHV_MAP_GPA_RANGE_FLAGS(WHvMapGpaRangeFlagRead.0 | WHvMapGpaRangeFlagWrite.0)
            }
            Access::ReadExecute => {
                WHV_MAP_GPA_RANGE_FLAGS(WHvMapGpaRangeFlagRead.0 | WHvMapGpaRangeFlagExecute.0)
            }
        }
    }
}

/// WHV_REGISTER_VALUE with the 16-byte alignment the C headers declare
/// (DECLSPEC_ALIGN(16) on WHV_UINT128). The Rust binding's type is only
/// 8-byte aligned, and the hypervisor faults on misaligned register arrays.
#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct AlignedValue(WHV_REGISTER_VALUE);

const _: () = assert!(std::mem::size_of::<AlignedValue>() == std::mem::size_of::<WHV_REGISTER_VALUE>());

/// A hypervisor partition (one VM).
pub struct Partition {
    handle: WHV_PARTITION_HANDLE,
}

// SAFETY: WHPX partition handles may be used from any thread; the API is
// documented as thread safe (e.g. cancelling a vCPU from another thread).
unsafe impl Send for Partition {}
unsafe impl Sync for Partition {}

impl Partition {
    /// Create and set up a partition with `cpus` processors, the local APIC
    /// emulated by the hypervisor (xAPIC mode), CPUID exits for
    /// `cpuid_leaves`, and INIT/SIPI IPIs trapped to the VMM (which re-issues
    /// them with [`Partition::request_interrupt`], as QEMU's WHPX backend does).
    pub fn new(cpus: u32, cpuid_leaves: &[u32]) -> Result<Self> {
        // SAFETY: plain FFI calls; every buffer passed matches the size given.
        unsafe {
            let handle = WHvCreatePartition().context("WHvCreatePartition")?;
            let part = Partition { handle };
            part.set_property(WHvPartitionPropertyCodeProcessorCount, &cpus)
                .context("set processor count")?;
            part.set_property(
                WHvPartitionPropertyCodeLocalApicEmulationMode,
                &WHvX64LocalApicEmulationModeXApic,
            )
            .context("set xAPIC emulation")?;
            // Extended VM exits: bit 0 X64CpuidExit, bit 6 X64ApicInitSipiExitTrap.
            let exits = WHV_EXTENDED_VM_EXITS { AsUINT64: 1 | 1 << 6 };
            part.set_property(WHvPartitionPropertyCodeExtendedVmExits, &exits)
                .context("enable CPUID exits")?;
            WHvSetPartitionProperty(
                handle,
                WHvPartitionPropertyCodeCpuidExitList,
                cpuid_leaves.as_ptr() as _,
                std::mem::size_of_val(cpuid_leaves) as u32,
            )
            .context("set CPUID exit list")?;
            WHvSetupPartition(handle).context("WHvSetupPartition")?;
            Ok(part)
        }
    }

    unsafe fn set_property<T>(
        &self,
        code: WHV_PARTITION_PROPERTY_CODE,
        value: &T,
    ) -> windows::core::Result<()> {
        WHvSetPartitionProperty(
            self.handle,
            code,
            value as *const T as _,
            std::mem::size_of::<T>() as u32,
        )
    }

    /// Map `size` bytes of host memory at `host` into the guest at `gpa`.
    ///
    /// # Safety
    /// `host..host+size` must stay valid, page aligned and unaliased by Rust
    /// references for as long as the mapping exists.
    pub unsafe fn map(&self, host: *mut u8, gpa: u64, size: u64, access: Access) -> Result<()> {
        WHvMapGpaRange(self.handle, host as _, gpa, size, access.flags()).context("WHvMapGpaRange")
    }

    pub fn unmap(&self, gpa: u64, size: u64) -> Result<()> {
        // SAFETY: unmapping only removes guest access.
        unsafe { WHvUnmapGpaRange(self.handle, gpa, size) }.context("WHvUnmapGpaRange")
    }

    pub fn create_vp(&self, index: u32) -> Result<()> {
        // SAFETY: plain FFI call on a valid partition.
        unsafe { WHvCreateVirtualProcessor(self.handle, index, 0) }
            .context("WHvCreateVirtualProcessor")
    }

    /// Run a vCPU until the next exit.
    pub fn run(&self, vp: u32) -> Result<WHV_RUN_VP_EXIT_CONTEXT> {
        // SAFETY: the exit context buffer has the size we pass.
        unsafe {
            let mut exit: WHV_RUN_VP_EXIT_CONTEXT = std::mem::zeroed();
            WHvRunVirtualProcessor(
                self.handle,
                vp,
                &mut exit as *mut _ as _,
                std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            )
            .context("WHvRunVirtualProcessor")?;
            Ok(exit)
        }
    }

    /// Make a running (or the next) `run` of `vp` return with a Canceled exit.
    pub fn cancel(&self, vp: u32) {
        // SAFETY: plain FFI call; failure (VP not running) is harmless.
        let _ = unsafe { WHvCancelRunVirtualProcessor(self.handle, vp, 0) };
    }

    pub fn get_regs(
        &self,
        vp: u32,
        names: &[WHV_REGISTER_NAME],
        values: &mut [WHV_REGISTER_VALUE],
    ) -> Result<()> {
        assert_eq!(names.len(), values.len());
        let mut buf = vec![AlignedValue::default(); values.len()];
        // SAFETY: names and buf both hold names.len() elements, and buf is
        // 16-byte aligned as the hypervisor requires.
        unsafe {
            WHvGetVirtualProcessorRegisters(
                self.handle,
                vp,
                names.as_ptr(),
                names.len() as u32,
                buf.as_mut_ptr() as *mut WHV_REGISTER_VALUE,
            )
        }
        .context("WHvGetVirtualProcessorRegisters")?;
        for (v, b) in values.iter_mut().zip(&buf) {
            *v = b.0;
        }
        Ok(())
    }

    pub fn set_regs(
        &self,
        vp: u32,
        names: &[WHV_REGISTER_NAME],
        values: &[WHV_REGISTER_VALUE],
    ) -> Result<()> {
        assert_eq!(names.len(), values.len());
        let buf: Vec<AlignedValue> = values.iter().map(|&v| AlignedValue(v)).collect();
        // SAFETY: as in get_regs.
        unsafe {
            WHvSetVirtualProcessorRegisters(
                self.handle,
                vp,
                names.as_ptr(),
                names.len() as u32,
                buf.as_ptr() as *const WHV_REGISTER_VALUE,
            )
        }
        .context("WHvSetVirtualProcessorRegisters")
    }

    /// Deliver an interrupt through the hypervisor's local APICs (used for
    /// INIT and SIPI). `kind` is a WHV_INTERRUPT_TYPE; destination mode
    /// physical unless `logical`.
    pub fn request_interrupt(&self, kind: WHV_INTERRUPT_TYPE, logical: bool, level: bool, destination: u32, vector: u32) -> Result<()> {
        // WHV_INTERRUPT_CONTROL: Type bits 0-7, DestinationMode 8-11, TriggerMode 12-15.
        let bits = (kind.0 as u64 & 0xff) | (u64::from(logical) << 8) | (u64::from(level) << 12);
        let ctl = WHV_INTERRUPT_CONTROL { _bitfield: bits, Destination: destination, Vector: vector };
        // SAFETY: ctl is a valid, correctly sized structure.
        unsafe {
            WHvRequestInterrupt(self.handle, &ctl, std::mem::size_of::<WHV_INTERRUPT_CONTROL>() as u32)
        }
        .context("WHvRequestInterrupt")
    }

    /// True if the vCPU's local APIC has any interrupt requested (IRR bit
    /// set). An error counts as pending, so a caller never waits forever.
    pub fn apic_irr_pending(&self, vp: u32) -> bool {
        let names = [
            WHvX64RegisterApicIrr0,
            WHvX64RegisterApicIrr1,
            WHvX64RegisterApicIrr2,
            WHvX64RegisterApicIrr3,
            WHvX64RegisterApicIrr4,
            WHvX64RegisterApicIrr5,
            WHvX64RegisterApicIrr6,
            WHvX64RegisterApicIrr7,
        ];
        let mut values = [WHV_REGISTER_VALUE::default(); 8];
        match self.get_regs(vp, &names, &mut values) {
            // SAFETY: the IRR registers are 64-bit values (low 32 bits used).
            Ok(()) => values.iter().any(|v| unsafe { v.Reg64 } & 0xffff_ffff != 0),
            Err(_) => true,
        }
    }

    pub fn get_reg(&self, vp: u32, name: WHV_REGISTER_NAME) -> Result<WHV_REGISTER_VALUE> {
        let mut v = [WHV_REGISTER_VALUE::default()];
        self.get_regs(vp, &[name], &mut v)?;
        Ok(v[0])
    }

    /// Translate a guest-virtual address with the vCPU's current paging state.
    pub fn translate_gva(
        &self,
        vp: u32,
        gva: u64,
        flags: WHV_TRANSLATE_GVA_FLAGS,
    ) -> Result<(WHV_TRANSLATE_GVA_RESULT_CODE, u64)> {
        let mut result = WHV_TRANSLATE_GVA_RESULT::default();
        let mut gpa = 0u64;
        // SAFETY: out-pointers are valid locals.
        unsafe { WHvTranslateGva(self.handle, vp, gva, flags, &mut result, &mut gpa) }
            .context("WHvTranslateGva")?;
        Ok((result.ResultCode, gpa))
    }
}

impl Drop for Partition {
    fn drop(&mut self) {
        // SAFETY: the handle is valid and no longer used after this.
        let _ = unsafe { WHvDeletePartition(self.handle) };
    }
}
