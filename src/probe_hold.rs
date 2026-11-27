//! The bench's debug probes, held open for a lease's or a study's whole life (decision 159).
//!
//! The same owners as [`crate::port_hold`]: a lease and a study each own the hold, and the
//! probes close when the last owner lets go. Each enrolled board's probe is opened through
//! probe-rs at the take, keyed by its serial number.
//!
//! **A Core call does not borrow the handle; it closes it and reopens it.** The board-identity
//! gate (`embarch-topology`) opens the probe itself, and probe-rs's `Session` consumes the
//! `Probe` it attaches and offers no way to hand it back, so neither can be given the held
//! handle. [`around`] closes the held probe, runs the call, and reopens it, on success and on
//! error. The window between the close and Core's own open is **not covered**, and the same
//! goes for a vendor tool's run (`nrfutil`, J-Link), which needs the probe for itself; both are
//! logged.
//!
//! A probe absent at the take is not held, and is not adopted later.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
#[cfg(not(test))]
use std::sync::Mutex;

use crate::port_hold::Owner;

/// What kind of call closes the held probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    /// Core's own probe-rs call: the gate and the attach each open the probe themselves.
    Core,
    /// A vendor backend's tool runs as a separate program that needs the probe itself.
    VendorTool,
}

/// An open probe. The holder only keeps it open and drops it; it never calls into it.
type Handle = Box<dyn Any + Send>;

#[cfg(test)]
type Opener = fn(&str) -> anyhow::Result<Handle>;

struct State {
    owners: BTreeSet<Owner>,
    /// `Some` while open; `None` while a call has it closed (it is reopened when the call ends).
    probes: BTreeMap<String, Option<Handle>>,
    /// Nesting depth of [`around`] per serial, so a vendor window inside a Core call reopens once.
    depth: BTreeMap<String, u32>,
}

impl State {
    const fn new() -> Self {
        State { owners: BTreeSet::new(), probes: BTreeMap::new(), depth: BTreeMap::new() }
    }
}

#[cfg(not(test))]
static STATE: Mutex<State> = Mutex::new(State::new());

#[cfg(test)]
thread_local! {
    static STATE: std::cell::RefCell<State> = const { std::cell::RefCell::new(State::new()) };
    static OPENER: std::cell::RefCell<Option<Opener>> = const { std::cell::RefCell::new(None) };
}

#[cfg(not(test))]
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    f(&mut STATE.lock().unwrap_or_else(|e| e.into_inner()))
}

#[cfg(test)]
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

#[cfg(not(test))]
fn open_one(serial: &str) -> anyhow::Result<Handle> {
    let probe = crate::hardware::open_probe(Some(serial), "hold")?;
    Ok(Box::new(probe))
}

#[cfg(test)]
fn open_one(serial: &str) -> anyhow::Result<Handle> {
    match OPENER.with(|o| *o.borrow()) {
        Some(f) => f(serial),
        None => anyhow::bail!("no probe opener under test"),
    }
}

/// The USB serial of every enrolled board's probe. Nothing under test.
fn declared_serials() -> Vec<String> {
    if cfg!(test) {
        return Vec::new();
    }
    match embarch_topology::hardware::list_enrolled() {
        Ok(boards) => boards.into_iter().filter_map(|b| b.probe_serial).collect(),
        Err(e) => {
            tracing::warn!("couldn't read the enrolled boards to hold their probes: {e:#}");
            Vec::new()
        }
    }
}

/// Makes `owner` a holder and opens every enrolled board's probe not already held.
pub fn acquire(owner: Owner) {
    acquire_with(owner, declared_serials());
}

/// [`acquire`] over an explicit serial list.
pub fn acquire_with(owner: Owner, serials: Vec<String>) {
    with_state(|st| {
        st.owners.insert(owner);
        for serial in serials {
            if st.probes.contains_key(&serial) {
                continue;
            }
            match open_one(&serial) {
                Ok(h) => {
                    tracing::info!("holding probe {serial} for the {owner:?}");
                    st.probes.insert(serial, Some(h));
                }
                Err(e) => tracing::warn!("couldn't hold probe {serial} at the take ({e:#}); it is not held"),
            }
        }
    });
}

/// Ends `owner`'s hold; the probes close when no owner is left.
pub fn release(owner: Owner) {
    with_state(|st| {
        if st.owners.remove(&owner) && st.owners.is_empty() {
            let n = st.probes.len();
            st.probes.clear();
            st.depth.clear();
            if n > 0 {
                tracing::info!("released {n} held probe(s)");
            }
        }
    });
}

/// The probe serials held now, sorted: what a signed record lists under `observed.probes_held`.
/// A probe a call has closed for its window still counts: Core reopens it when the call ends.
pub fn held() -> Vec<String> {
    with_state(|st| st.probes.keys().cloned().collect())
}

/// Runs `f` with `serial`'s held probe closed, then reopens it, on success and on error. A
/// serial not held runs `f` as before. Nested calls on one serial close and reopen once.
pub fn around<T>(serial: &str, kind: Use, f: impl FnOnce() -> T) -> T {
    let entered = with_state(|st| {
        let Some(slot) = st.probes.get_mut(serial) else { return false };
        let d = st.depth.entry(serial.to_string()).or_insert(0);
        *d += 1;
        let outermost = *d == 1;
        if outermost {
            *slot = None;
        }
        match kind {
            Use::Core => {
                if outermost {
                    tracing::info!("probe {serial} closed for Core's own call; not covered until Core opens it");
                }
            }
            Use::VendorTool => tracing::warn!(
                "probe {serial} is not held while the vendor tool runs; a second connection can take it until Core reopens it"
            ),
        }
        true
    });
    let out = f();
    if entered {
        with_state(|st| {
            let Some(d) = st.depth.get_mut(serial) else { return };
            *d -= 1;
            if *d > 0 {
                return;
            }
            st.depth.remove(serial);
            // The hold ended during the call (release): nothing to reopen.
            let Some(slot) = st.probes.get_mut(serial) else { return };
            match open_one(serial) {
                Ok(h) => {
                    tracing::info!("probe {serial} reopened and held again");
                    *slot = Some(h);
                }
                Err(e) => {
                    tracing::warn!("couldn't reopen probe {serial} after the call ({e:#}); it is no longer held");
                    st.probes.remove(serial);
                }
            }
        });
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        static OPENS: Cell<u32> = const { Cell::new(0) };
        static LIVE: Cell<i32> = const { Cell::new(0) };
    }

    struct Fake;
    impl Drop for Fake {
        fn drop(&mut self) {
            LIVE.with(|l| l.set(l.get() - 1));
        }
    }

    fn fake_open(_serial: &str) -> anyhow::Result<Handle> {
        OPENS.with(|o| o.set(o.get() + 1));
        LIVE.with(|l| l.set(l.get() + 1));
        Ok(Box::new(Fake))
    }

    pub(crate) fn use_fake_opener() {
        OPENS.with(|o| o.set(0));
        LIVE.with(|l| l.set(0));
        OPENER.with(|o| *o.borrow_mut() = Some(fake_open));
    }

    fn opens() -> u32 {
        OPENS.with(|o| o.get())
    }
    fn live() -> i32 {
        LIVE.with(|l| l.get())
    }

    #[test]
    fn a_probe_opens_once_at_the_take_stays_open_across_calls_and_closes_at_the_release() {
        use_fake_opener();
        acquire_with(Owner::Lease, vec!["P1".into()]);
        assert_eq!((opens(), live()), (1, 1));
        assert_eq!(held(), vec!["P1".to_string()]);

        // A mem-read and a flash each close it for their window and reopen it.
        for _ in 0..2 {
            around("P1", Use::Core, || assert_eq!(live(), 0, "closed while Core's call runs"));
            assert_eq!(live(), 1, "held again after the call");
        }
        assert_eq!(opens(), 3);

        release(Owner::Lease);
        assert_eq!(live(), 0);
        assert!(held().is_empty());
    }

    #[test]
    fn a_vendor_tool_window_closes_the_probe_before_the_tool_and_reopens_it_after() {
        use_fake_opener();
        acquire_with(Owner::Lease, vec!["P1".into()]);
        let r: Result<(), &str> = around("P1", Use::Core, || {
            around("P1", Use::VendorTool, || {
                assert_eq!(live(), 0, "closed before the tool runs");
                Err("tool failed")
            })
        });
        assert!(r.is_err());
        assert_eq!((live(), opens()), (1, 2), "reopened once, after the tool, even on error");
        release(Owner::Lease);
    }

    #[test]
    fn the_probes_close_only_when_the_last_owner_lets_go() {
        use_fake_opener();
        acquire_with(Owner::Lease, vec!["P1".into()]);
        acquire_with(Owner::Study, vec!["P1".into()]);
        assert_eq!(opens(), 1, "a second owner shares the hold");
        release(Owner::Study);
        assert_eq!(held(), vec!["P1".to_string()]);
        release(Owner::Lease);
        assert_eq!(live(), 0);
    }

    #[test]
    fn a_probe_not_held_runs_the_call_untouched_and_a_release_during_a_call_is_not_undone() {
        use_fake_opener();
        assert_eq!(around("PX", Use::Core, || 7), 7);
        assert_eq!(opens(), 0);
        acquire_with(Owner::Lease, vec!["P1".into()]);
        around("P1", Use::Core, || release(Owner::Lease));
        assert_eq!(live(), 0);
        assert!(held().is_empty());
    }
}
