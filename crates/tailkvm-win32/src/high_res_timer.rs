//! Process-local high-resolution pacing timer.
//!
//! `tokio::time::sleep` and `std::thread::sleep` inherit the system timer
//! granularity (~15.6ms by default), so sub-16ms pacing is imprecise unless the
//! global timer resolution is raised — and raising it globally (`timeBeginPeriod`)
//! slows the whole machine. A high-resolution *waitable timer*
//! (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`, Windows 10 1803+) gives ~1ms
//! precision for THIS timer only, with no system-wide cost. Falls back to
//! `std::thread::sleep` if the high-resolution timer cannot be created or armed.

use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED};
use windows_sys::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
};

/// A reusable high-resolution waitable timer. Not `Send`: create and use it on
/// the single thread that performs the pacing.
pub struct HighResTimer {
    handle: HANDLE,
}

impl HighResTimer {
    /// Create a high-resolution waitable timer. If the OS does not support the
    /// high-resolution flag (pre-1803) the handle is null and `wait_ms` falls
    /// back to `std::thread::sleep`.
    pub fn new() -> Self {
        // SAFETY: documented Win32 call. Null security attributes and a null
        // name create an unnamed auto-reset timer; we own the returned handle
        // and release it in `Drop`. Returns null on failure, which we tolerate.
        let handle = unsafe {
            CreateWaitableTimerExW(
                core::ptr::null(),
                core::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        Self { handle }
    }

    /// Block the current thread for approximately `ms` milliseconds with high
    /// precision. Falls back to `std::thread::sleep` if the timer is
    /// unavailable or arming fails. `ms == 0` returns immediately.
    pub fn wait_ms(&self, ms: u64) {
        if ms == 0 {
            return;
        }
        if self.handle.is_null() {
            std::thread::sleep(Duration::from_millis(ms));
            return;
        }
        // Negative due time = relative interval, expressed in 100ns units.
        let due: i64 = -(ms as i64).saturating_mul(10_000);
        // SAFETY: `self.handle` is a valid timer we created; `due` points to a
        // live local i64; no completion routine is used.
        let armed = unsafe { SetWaitableTimer(self.handle, &due, 0, None, core::ptr::null(), 0) };
        if armed == 0 {
            std::thread::sleep(Duration::from_millis(ms));
            return;
        }
        // Bounded wait: a normally-firing timer returns WAIT_OBJECT_0 after ~ms.
        // The slack caps a misfiring timer (returns WAIT_TIMEOUT) so the capture
        // loop can never hang on it.
        // SAFETY: waiting on our own valid auto-reset timer handle.
        let waited = unsafe { WaitForSingleObject(self.handle, (ms as u32).saturating_add(50)) };
        if waited == WAIT_FAILED {
            // The wait could not be performed at all — fall back to a sleep so
            // the loop keeps its cadence instead of busy-spinning.
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
}

impl Default for HighResTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HighResTimer {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: `handle` was created in `new` and is not closed elsewhere.
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}
