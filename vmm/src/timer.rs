//! Sub-millisecond sleeps. `std::thread::sleep` rounds up to the system
//! timer tick (often 15.6 ms), far too coarse for a 1 kHz guest timer, so
//! this uses a high-resolution waitable timer (Windows 10 1803+).

use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
};

pub struct HrTimer {
    handle: HANDLE,
}

// SAFETY: a timer handle may be used from any thread; each HrTimer is only
// waited on by its owner.
unsafe impl Send for HrTimer {}

impl HrTimer {
    pub fn new() -> Self {
        // SAFETY: plain FFI calls creating a fresh, unnamed timer.
        let handle = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
            .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0))
            .expect("CreateWaitableTimerExW")
        };
        HrTimer { handle }
    }

    pub fn sleep(&self, d: Duration) {
        if d.is_zero() {
            return;
        }
        // Negative due time = relative, in 100 ns units.
        let due = -((d.as_nanos() / 100).max(1) as i64);
        // SAFETY: the handle is a valid timer owned by self.
        unsafe {
            if SetWaitableTimer(self.handle, &due, 0, None, None, false).is_ok() {
                WaitForSingleObject(self.handle, INFINITE);
            }
        }
    }
}

impl Default for HrTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HrTimer {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by self and closed once.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}
