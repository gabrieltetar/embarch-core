//! Keeping the host awake while a Test run holds the lease (decision 151).
//!
//! The OS call sits behind [`KeepAwake`] so the hold and the release are tested without it. A
//! [`Hold`] takes the hold when made and gives it back when dropped, so every way a run ends -
//! completed, cancelled, an error, a panic - releases it.

/// Asks the host to stay awake, and stops asking.
pub trait KeepAwake {
    /// Asks the host to stay awake.
    fn hold(&mut self);
    /// Gives the request back.
    fn release(&mut self);
}

/// One run's request to keep the host awake; released on drop.
pub struct Hold<'a>(&'a mut dyn KeepAwake);

impl<'a> Hold<'a> {
    pub fn new(keeper: &'a mut dyn KeepAwake) -> Self {
        keeper.hold();
        Hold(keeper)
    }
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Windows: `SetThreadExecutionState` with `ES_SYSTEM_REQUIRED`. The state belongs to the calling
/// thread, so the run's own thread holds and releases it. Not measured: whether it keeps a
/// service's host awake through an idle timeout (decision 151).
#[cfg(windows)]
pub struct HostKeepAwake;

#[cfg(windows)]
impl KeepAwake for HostKeepAwake {
    fn hold(&mut self) {
        use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};
        // SAFETY: a plain flags call with no pointers; it returns the previous state, 0 on failure.
        if unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) } == 0 {
            tracing::warn!("could not ask Windows to keep the host awake for the run");
        }
    }
    fn release(&mut self) {
        use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS};
        // SAFETY: as above; `ES_CONTINUOUS` alone clears the requirement this thread set.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    }
}

/// Elsewhere: nothing to ask, and it says so once per run.
#[cfg(not(windows))]
pub struct HostKeepAwake;

#[cfg(not(windows))]
impl KeepAwake for HostKeepAwake {
    fn hold(&mut self) {
        tracing::info!("this host is not Windows: Core does not keep it awake for the run");
    }
    fn release(&mut self) {}
}

#[cfg(test)]
pub mod testing {
    use super::KeepAwake;
    use std::sync::{Arc, Mutex};

    /// Records `hold` and `release` in order.
    #[derive(Clone, Default)]
    pub struct Recorder(pub Arc<Mutex<Vec<&'static str>>>);

    impl KeepAwake for Recorder {
        fn hold(&mut self) {
            self.0.lock().unwrap().push("hold");
        }
        fn release(&mut self) {
            self.0.lock().unwrap().push("release");
        }
    }
}
