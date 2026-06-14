//! Hold the Windows multimedia timer resolution high for the duration of an
//! input session.
//!
//! Windows' default timer granularity is ~15.6ms. `tokio::time::sleep` and the
//! thread scheduler quantum inherit that granularity, so sub-16ms polling is
//! imprecise — and, worst of all, right after an idle period the OS coarsens
//! timers for power saving, which surfaces as a large hitch when input resumes
//! after the cursor has been static for a while. Holding a 1ms resolution while
//! capture (or a controlled receiver session) is active keeps the poll/inject
//! cadence steady and removes that idle->active stutter.

use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};

/// RAII guard that raises the system timer resolution to `period_ms` and
/// restores it on drop. Construct one for the lifetime of an input session
/// (controller capture loop or receiver session) and let it drop on any exit
/// path — `Drop` pairs the `timeEndPeriod` with the `timeBeginPeriod`.
#[derive(Debug)]
pub struct TimerResolutionGuard {
    period_ms: u32,
    active: bool,
}

impl TimerResolutionGuard {
    /// Request `period_ms` (e.g. `1`) timer resolution. If the request fails the
    /// guard is inert and `Drop` is a no-op, so callers can ignore failures.
    pub fn new(period_ms: u32) -> Self {
        // SAFETY: `timeBeginPeriod` is a documented winmm call taking a period
        // in milliseconds; it returns TIMERR_NOERROR (0) on success. Every
        // successful call is paired with exactly one `timeEndPeriod` in `Drop`.
        let active = unsafe { timeBeginPeriod(period_ms) } == 0;
        Self { period_ms, active }
    }
}

impl Drop for TimerResolutionGuard {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: paired one-to-one with the successful `timeBeginPeriod`
            // in `new`; restores the resolution period we requested.
            unsafe {
                timeEndPeriod(self.period_ms);
            }
        }
    }
}
