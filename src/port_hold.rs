//! The bench's serial ports, held open for a lease's or a study's whole life (decision 130).
//!
//! `serialport` opens a POSIX port exclusively (TIOCEXCL), so a handle Core keeps open keeps a
//! second opener out, including a second Core on another HTTP port. Before this module every
//! serial use opened, used and closed, so that exclusion held only during a call.
//!
//! **Owners.** A lease and a study each own the hold; the ports close when the last owner lets
//! go. A study with no lease holds for its own run.
//!
//! **Borrowing.** [`open`] is the one door every Core serial use goes through. A held port is
//! lent out, one borrower at a time (`hw_lock` already serializes callers; a second borrower
//! gets an error rather than a wait), reconfigured to the caller's baud, timeout and no flow
//! control; dropping the [`Lent`] hands it back. A port with no hold is opened fresh and closed
//! on drop, as before.
//!
//! **A port absent at the take** is logged and held from its first use: while an owner is
//! live, any port opened through [`open`] is adopted on return.
//!
//! Not held, by design: bootload ports (they re-enumerate during a bootload, so a handle would
//! go stale), and the dev-bench link and the analog samples port, which a study opens itself
//! for its whole life with its own flow control.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
#[cfg(not(test))]
use std::sync::Mutex;
use std::time::Duration;

use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, StopBits};

/// Who is keeping the ports held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    Lease,
    Study,
}

struct State {
    owners: BTreeSet<Owner>,
    /// `None` while a borrower has the handle.
    ports: BTreeMap<String, Option<Box<dyn SerialPort>>>,
}

#[cfg(not(test))]
static STATE: Mutex<State> = Mutex::new(State { owners: BTreeSet::new(), ports: BTreeMap::new() });

// Under test each test thread has its own holder, so one test's lease cannot close another's
// ports.
#[cfg(test)]
thread_local! {
    static STATE: std::cell::RefCell<State> = const { std::cell::RefCell::new(State { owners: BTreeSet::new(), ports: BTreeMap::new() }) };
}

#[cfg(not(test))]
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    f(&mut STATE.lock().unwrap_or_else(|e| e.into_inner()))
}

#[cfg(test)]
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

/// Every declared direct signal's and instrument's port that is enumerable now, with the baud
/// its declaration names. A port that does not resolve is skipped (held from first use).
pub fn declared_targets() -> Vec<(String, u32)> {
    // A test must never open a port the machine it runs on declares.
    if cfg!(test) {
        return Vec::new();
    }
    let mut out: Vec<(String, u32)> = Vec::new();
    match embarch_topology::hardware::list_signals() {
        Ok(links) => {
            for link in links {
                if matches!(link.route, embarch_topology::hardware::Route::ViaDevBench { .. }) {
                    continue;
                }
                match embarch_topology::hardware::resolve_signal_port(&link.name) {
                    Ok(p) => out.push((p.port_name, crate::stream_store::signal_baud_for(&link))),
                    Err(e) => tracing::info!("signal '{}' has no port to hold now: {e:#}", link.name),
                }
            }
        }
        Err(e) => tracing::warn!("couldn't read the signal table to hold its ports: {e:#}"),
    }
    match embarch_topology::hardware::instrument::list() {
        Ok(list) => {
            for inst in list {
                match embarch_topology::hardware::instrument::find_port(&inst) {
                    Ok(Some(p)) => out.push((p.port_name, inst.baud)),
                    Ok(None) => tracing::info!("instrument '{}' has no port to hold now", inst.name),
                    Err(e) => tracing::info!("instrument '{}' has no port to hold now: {e:#}", inst.name),
                }
            }
        }
        Err(e) => tracing::warn!("couldn't read the instrument table to hold its ports: {e:#}"),
    }
    out
}

/// Makes `owner` a holder and opens every declared port not already held.
pub fn acquire(owner: Owner) {
    crate::probe_hold::acquire(owner);
    acquire_with(owner, declared_targets());
}

/// [`acquire`] over an explicit port list.
pub fn acquire_with(owner: Owner, targets: Vec<(String, u32)>) {
    with_state(|st| {
        st.owners.insert(owner);
        for (name, baud) in targets {
            if st.ports.contains_key(&name) {
                continue;
            }
            match serialport::new(&name, baud).timeout(crate::exchange::READ_POLL).open() {
                Ok(p) => {
                    tracing::info!("holding {name} for the {owner:?}");
                    st.ports.insert(name, Some(p));
                }
                Err(e) => tracing::warn!("couldn't hold {name} at the take ({e}); it is held from its first use"),
            }
        }
    });
}

/// Ends `owner`'s hold; the ports close when no owner is left.
pub fn release(owner: Owner) {
    crate::probe_hold::release(owner);
    with_state(|st| {
        if st.owners.remove(&owner) && st.owners.is_empty() {
            let n = st.ports.len();
            st.ports.clear();
            if n > 0 {
                tracing::info!("released {n} held port(s)");
            }
        }
    });
}

/// The ports held now, sorted: what a signed record lists under `observed.ports_held`.
pub fn held() -> Vec<String> {
    with_state(|st| st.ports.keys().cloned().collect())
}

/// A serial port borrowed from the hold, or opened fresh when none holds it.
struct Lent {
    name: String,
    port: Option<Box<dyn SerialPort>>,
}

/// Opens `name` at `baud`: borrows the held handle, or opens a fresh one. Errors when the
/// handle is out with another borrower, or the OS refuses the open.
pub fn open(name: &str, baud: u32, timeout: Duration) -> serialport::Result<Box<dyn SerialPort>> {
    let borrowed = with_state(|st| st.ports.get_mut(name).map(|slot| slot.take()));
    let port = match borrowed {
        Some(Some(p)) => p,
        Some(None) => {
            return Err(serialport::Error::new(
                serialport::ErrorKind::Io(std::io::ErrorKind::WouldBlock),
                format!("{name} is held by Core and another borrower has it"),
            ))
        }
        None => serialport::new(name, baud).timeout(timeout).open()?,
    };
    let mut lent = Lent { name: name.to_string(), port: Some(port) };
    lent.set_baud_rate(baud)?;
    lent.set_flow_control(FlowControl::None)?;
    lent.set_timeout(timeout)?;
    Ok(Box::new(lent))
}

impl Drop for Lent {
    fn drop(&mut self) {
        let Some(p) = self.port.take() else { return };
        let name = std::mem::take(&mut self.name);
        with_state(|st| {
            if st.ports.contains_key(&name) || !st.owners.is_empty() {
                st.ports.insert(name, Some(p));
            }
        });
    }
}

impl Lent {
    fn p(&self) -> &dyn SerialPort {
        self.port.as_deref().expect("a Lent holds its port until dropped")
    }
    fn pm(&mut self) -> &mut dyn SerialPort {
        self.port.as_deref_mut().expect("a Lent holds its port until dropped")
    }
}

impl Read for Lent {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.pm().read(buf)
    }
}

impl Write for Lent {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pm().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.pm().flush()
    }
}

impl SerialPort for Lent {
    fn name(&self) -> Option<String> {
        self.p().name()
    }
    fn baud_rate(&self) -> serialport::Result<u32> {
        self.p().baud_rate()
    }
    fn data_bits(&self) -> serialport::Result<DataBits> {
        self.p().data_bits()
    }
    fn flow_control(&self) -> serialport::Result<FlowControl> {
        self.p().flow_control()
    }
    fn parity(&self) -> serialport::Result<Parity> {
        self.p().parity()
    }
    fn stop_bits(&self) -> serialport::Result<StopBits> {
        self.p().stop_bits()
    }
    fn timeout(&self) -> Duration {
        self.p().timeout()
    }
    fn set_baud_rate(&mut self, baud_rate: u32) -> serialport::Result<()> {
        self.pm().set_baud_rate(baud_rate)
    }
    fn set_data_bits(&mut self, data_bits: DataBits) -> serialport::Result<()> {
        self.pm().set_data_bits(data_bits)
    }
    fn set_flow_control(&mut self, flow_control: FlowControl) -> serialport::Result<()> {
        self.pm().set_flow_control(flow_control)
    }
    fn set_parity(&mut self, parity: Parity) -> serialport::Result<()> {
        self.pm().set_parity(parity)
    }
    fn set_stop_bits(&mut self, stop_bits: StopBits) -> serialport::Result<()> {
        self.pm().set_stop_bits(stop_bits)
    }
    fn set_timeout(&mut self, timeout: Duration) -> serialport::Result<()> {
        self.pm().set_timeout(timeout)
    }
    fn write_request_to_send(&mut self, level: bool) -> serialport::Result<()> {
        self.pm().write_request_to_send(level)
    }
    fn write_data_terminal_ready(&mut self, level: bool) -> serialport::Result<()> {
        self.pm().write_data_terminal_ready(level)
    }
    fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
        self.pm().read_clear_to_send()
    }
    fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
        self.pm().read_data_set_ready()
    }
    fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
        self.pm().read_ring_indicator()
    }
    fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
        self.pm().read_carrier_detect()
    }
    fn bytes_to_read(&self) -> serialport::Result<u32> {
        self.p().bytes_to_read()
    }
    fn bytes_to_write(&self) -> serialport::Result<u32> {
        self.p().bytes_to_write()
    }
    fn clear(&self, buffer_to_clear: ClearBuffer) -> serialport::Result<()> {
        self.p().clear(buffer_to_clear)
    }
    fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
        self.p().try_clone()
    }
    fn set_break(&self) -> serialport::Result<()> {
        self.p().set_break()
    }
    fn clear_break(&self) -> serialport::Result<()> {
        self.p().clear_break()
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;

    const HOLD_BAUD: u32 = 115_200;

    /// A pty pair: the master stays open for the test's life, the slave's path is returned.
    pub(crate) fn pty() -> (std::os::fd::OwnedFd, String) {
        use std::os::fd::FromRawFd;
        let (mut m, mut s) = (0, 0);
        let mut buf = [0 as libc::c_char; 128];
        // SAFETY: openpty fills the two fds and the name buffer, which is 128 bytes.
        let rc = unsafe { libc::openpty(&mut m, &mut s, buf.as_mut_ptr(), std::ptr::null(), std::ptr::null()) };
        assert_eq!(rc, 0, "openpty");
        // SAFETY: `s` is a fresh fd we own; closing it leaves the master and the slave path.
        unsafe { libc::close(s) };
        let name = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap().to_string();
        // SAFETY: `m` is a fresh fd we own.
        (unsafe { std::os::fd::OwnedFd::from_raw_fd(m) }, name)
    }

    fn fresh_open(name: &str) -> serialport::Result<Box<dyn SerialPort>> {
        serialport::new(name, HOLD_BAUD).timeout(Duration::from_millis(50)).open()
    }

    #[test]
    fn a_held_port_keeps_a_second_opener_out_and_still_serves_core() {
        let (master, name) = pty();
        acquire_with(Owner::Lease, vec![(name.clone(), HOLD_BAUD)]);
        assert_eq!(held(), vec![name.clone()]);
        assert!(fresh_open(&name).is_err(), "a second open of a held port must fail");

        // Core's own use borrows the held handle; the bytes reach the master.
        let mut lent = open(&name, 9600, Duration::from_millis(100)).unwrap();
        assert_eq!(lent.baud_rate().unwrap(), 9600);
        lent.write_all(b"ping\n").unwrap();
        lent.flush().unwrap();
        let mut got = [0u8; 16];
        let mut m = std::fs::File::from(master.try_clone().unwrap());
        let n = m.read(&mut got).unwrap();
        assert!(std::str::from_utf8(&got[..n]).unwrap().contains("ping"));
        assert!(open(&name, 9600, Duration::from_millis(100)).is_err(), "one borrower at a time");
        drop(lent);
        assert!(fresh_open(&name).is_err(), "returning the handle keeps the port held");
        assert!(open(&name, 9600, Duration::from_millis(100)).is_ok(), "the handle is back");

        release(Owner::Lease);
        assert!(held().is_empty());
        assert!(fresh_open(&name).is_ok(), "after the release a second open succeeds");
    }

    #[test]
    fn the_ports_close_only_when_the_last_owner_lets_go() {
        let (_master, name) = pty();
        acquire_with(Owner::Lease, vec![(name.clone(), HOLD_BAUD)]);
        acquire_with(Owner::Study, vec![(name.clone(), HOLD_BAUD)]);
        release(Owner::Study);
        assert_eq!(held(), vec![name.clone()], "the lease still holds it");
        release(Owner::Lease);
        assert!(held().is_empty());
    }

    #[test]
    fn a_port_absent_at_the_take_is_held_from_its_first_use() {
        let (_master, name) = pty();
        acquire_with(Owner::Lease, vec![("/dev/does-not-exist-embarch".to_string(), HOLD_BAUD)]);
        assert!(held().is_empty());
        drop(open(&name, HOLD_BAUD, Duration::from_millis(50)).unwrap());
        assert_eq!(held(), vec![name.clone()]);
        assert!(fresh_open(&name).is_err());
        release(Owner::Lease);
        assert!(fresh_open(&name).is_ok());
    }

    #[test]
    fn with_no_owner_a_use_opens_and_closes() {
        let (_master, name) = pty();
        drop(open(&name, HOLD_BAUD, Duration::from_millis(50)).unwrap());
        assert!(held().is_empty());
        assert!(fresh_open(&name).is_ok());
    }
}
