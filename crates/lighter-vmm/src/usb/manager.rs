//! Which USB devices the guest has, kept in line with what is plugged in.
//!
//! The wanted list is the user's (`lighter usb attach`); the Mac decides what
//! is present. One thread reconciles the two, woken by a device arriving, by
//! the list changing, and every two seconds besides. A wanted device that is
//! present and not served is seized and attached; one no longer wanted is
//! detached. A session that ends soon after it began (the guest's agent not
//! listening yet at boot, or a device macOS will not let go of) is retried
//! with a back-off, so nothing spins.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::iousb::{self, IoUsbDevice, Info};
use super::server::{Server, Sink};

/// The vsock port the agent attaches devices on (`guest/agent/src/usb.rs`).
pub const USB_PORT: u32 = 2384;
const TICK: Duration = Duration::from_secs(2);
const FIRST_RETRY: Duration = Duration::from_millis(500);
const LONGEST_RETRY: Duration = Duration::from_secs(10);
/// A session that lasted less than this ended before it was really in use.
const SHORT_LIVED: Duration = Duration::from_secs(5);

/// A device the user asked for: vendor and product, and a serial number
/// when two of the same are plugged in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Spec {
    pub vendor: u16,
    pub product: u16,
    pub serial: Option<String>,
}

impl std::str::FromStr for Spec {
    type Err = String;
    fn from_str(s: &str) -> Result<Spec, String> {
        let mut parts = s.splitn(3, ':');
        let hex = |p: Option<&str>| {
            p.filter(|p| !p.is_empty() && p.len() <= 4)
                .and_then(|p| u16::from_str_radix(p, 16).ok())
                .ok_or_else(|| format!("'{s}' is not vendor:product[:serial], in hex (like 303a:831a)"))
        };
        let vendor = hex(parts.next())?;
        let product = hex(parts.next())?;
        let serial = parts.next().filter(|s| !s.is_empty()).map(str::to_owned);
        Ok(Spec { vendor, product, serial })
    }
}

impl std::fmt::Display for Spec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vendor, self.product)?;
        if let Some(serial) = &self.serial {
            write!(f, ":{serial}")?;
        }
        Ok(())
    }
}

impl Spec {
    pub fn matches(&self, info: &Info) -> bool {
        info.vendor_id == self.vendor
            && info.product_id == self.product
            && self.serial.as_ref().is_none_or(|s| *s == info.serial)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub spec: Spec,
    /// Attach even what is refused by default: an input device, storage, or
    /// a serial port a Mac program has open.
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Served to the guest.
    Attached,
    /// Not plugged in; attached when it is.
    Waiting,
    /// Present but not attached, and why; tried again on its own only if
    /// the reason can pass (a port in use), never for a policy refusal.
    Refused(String),
    /// Attaching, or trying again after a session that ended early.
    Retrying(String),
}

/// The record of held devices: registry ids, one per line. Written whole and
/// renamed into place, so a reader never sees half of it.
fn write_held(path: &Path, ids: &[u64]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let body: String = ids.iter().map(|id| format!("{id}\n")).collect();
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Gives back to macOS every device the record lists: a lighter that died
/// holding them left them seized and unconfigured, invisible to macOS until
/// replugged. Run by the keeper when the machine's process exits, and by
/// the next machine at start. A lock keeps the two apart, since two seizes
/// of one device take it from each other.
pub fn restore_held(path: &Path) {
    let Ok(lock) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path.with_extension("lock")) else {
        return;
    };
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a descriptor we hold for the whole function.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let ids: Vec<u64> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    for id in &ids {
        match iousb::restore(*id) {
            Ok(()) => tracing::info!(registry_id = id, "usb: gave a device back to macOS"),
            Err(e) => tracing::warn!(registry_id = id, "usb: could not give a device back to macOS: {e}"),
        }
    }
    let _ = write_held(path, &[]);
    // SAFETY: as above.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
}

/// Why a device is not attached without `force`, or `None`.
pub fn refusal(info: &Info) -> Option<String> {
    if info.has_interface_class(0xe0) {
        return Some("a Bluetooth adapter: not supported yet".into());
    }
    if info.device_class == iousb::CLASS_HUB || info.has_interface_class(iousb::CLASS_HUB) {
        return Some("a USB hub: attach the devices behind it instead".into());
    }
    if info.has_interface_class(iousb::CLASS_HID) {
        return Some("an input device (keyboard, mouse or similar); --force to attach it anyway".into());
    }
    if info.has_interface_class(iousb::CLASS_MASS_STORAGE) {
        return Some("a storage device, which macOS may have mounted; --force to attach it anyway".into());
    }
    None
}

struct Session {
    sink: Sink,
    registry_id: u64,
    started: Instant,
}

struct State {
    wanted: Vec<Want>,
    sessions: HashMap<Spec, Session>,
    /// Per spec: the next attempt's earliest time, and the pause after it.
    retry: HashMap<Spec, (Instant, Duration)>,
    status: HashMap<Spec, Status>,
    next_devid: u32,
    changed: bool,
    /// The registry ids last written to the held record.
    recorded: Vec<u64>,
}

pub struct Manager {
    /// Where the devices this process holds are recorded, for whoever gives
    /// them back if it dies holding them (`restore_held`).
    held: PathBuf,
    server: Server,
    reactor: Arc<crate::reactor::Reactor>,
    state: Mutex<State>,
    wake: Condvar,
}

impl Manager {
    pub fn start(reactor: Arc<crate::reactor::Reactor>, held: PathBuf) -> std::io::Result<Arc<Manager>> {
        // Devices a lighter that crashed left seized go back to macOS first;
        // the wanted ones are seized again below.
        restore_held(&held);
        let manager = Arc::new(Manager {
            held,
            server: Server::start()?,
            reactor,
            state: Mutex::new(State {
                wanted: Vec::new(),
                sessions: HashMap::new(),
                retry: HashMap::new(),
                status: HashMap::new(),
                next_devid: 0x0001_0001,
                changed: false,
                recorded: Vec::new(),
            }),
            wake: Condvar::new(),
        });
        let woken = Arc::downgrade(&manager);
        iousb::watch(Box::new(move || {
            if let Some(m) = woken.upgrade() {
                m.poke();
            }
        }));
        let looped = manager.clone();
        std::thread::Builder::new()
            .name("usb-manager".into())
            .spawn(move || looped.run())?;
        Ok(manager)
    }

    /// Replaces the wanted list; the difference takes effect at once.
    pub fn set(&self, wanted: Vec<Want>) {
        let mut state = self.state.lock().expect("usb state poisoned");
        state.wanted = wanted;
        state.retry.clear();
        state.changed = true;
        drop(state);
        self.wake.notify_one();
    }

    /// Each wanted device and where it stands, reconciled first.
    pub fn status(&self) -> Vec<(Spec, Status)> {
        let mut state = self.state.lock().expect("usb state poisoned");
        self.reconcile(&mut state);
        state
            .wanted
            .iter()
            .map(|w| (w.spec.clone(), state.status.get(&w.spec).cloned().unwrap_or(Status::Waiting)))
            .collect()
    }

    fn poke(&self) {
        self.state.lock().expect("usb state poisoned").changed = true;
        self.wake.notify_one();
    }

    fn run(self: Arc<Self>) {
        let mut state = self.state.lock().expect("usb state poisoned");
        loop {
            state.changed = false;
            self.reconcile(&mut state);
            let (s, _) = self
                .wake
                .wait_timeout_while(state, TICK, |s| !s.changed)
                .expect("usb state poisoned");
            state = s;
        }
    }

    fn reconcile(&self, state: &mut State) {
        self.reconcile_devices(state);
        let mut held: Vec<u64> = state.sessions.values().map(|s| s.registry_id).collect();
        held.sort_unstable();
        if held != state.recorded {
            match write_held(&self.held, &held) {
                Ok(()) => state.recorded = held,
                Err(e) => tracing::warn!("usb: cannot record the devices held: {e}"),
            }
        }
    }

    fn reconcile_devices(&self, state: &mut State) {
        let present = iousb::list();
        let now = Instant::now();
        // Detach what is no longer wanted.
        let wanted: Vec<Spec> = state.wanted.iter().map(|w| w.spec.clone()).collect();
        let unwanted: Vec<Spec> = state.sessions.keys().filter(|s| !wanted.contains(s)).cloned().collect();
        for spec in unwanted {
            if let Some(session) = state.sessions.remove(&spec) {
                self.server.end(&session.sink);
                tracing::info!(%spec, "usb: detached");
            }
            state.status.remove(&spec);
            state.retry.remove(&spec);
        }
        for want in state.wanted.clone() {
            let spec = want.spec.clone();
            // A session that ended: soon after it began is a failure to
            // back off from, and one that ran is a device unplugged or a
            // guest restarted, retried at once.
            if let Some(session) = state.sessions.get(&spec)
                && !self.server.serving(&session.sink)
            {
                let short = now.duration_since(session.started) < SHORT_LIVED;
                state.sessions.remove(&spec);
                let pause = match state.retry.get(&spec) {
                    Some(&(_, p)) if short => (p * 2).min(LONGEST_RETRY),
                    _ => FIRST_RETRY,
                };
                state.retry.insert(spec.clone(), (now + if short { pause } else { Duration::ZERO }, pause));
            }
            let Some(info) = present.iter().find(|i| spec.matches(i)) else {
                state.status.insert(spec, Status::Waiting);
                continue;
            };
            if let Some(session) = state.sessions.get(&spec) {
                // Served, and still the same device (a re-plugged one is new).
                if session.registry_id == info.registry_id {
                    state.status.insert(spec, Status::Attached);
                    continue;
                }
                self.server.end(&session.sink);
                state.sessions.remove(&spec);
            }
            if !want.force {
                if let Some(why) = refusal(info) {
                    state.status.insert(spec, Status::Refused(why));
                    continue;
                }
                if let Some(port) = &info.callout
                    && let Some((pid, name)) = iousb::port_holder(port)
                {
                    state.status.insert(
                        spec,
                        Status::Refused(format!("{port} is open in {name} (pid {pid}); quit it, or --force")),
                    );
                    continue;
                }
            }
            if let Some(&(at, _)) = state.retry.get(&spec)
                && now < at
            {
                if !matches!(state.status.get(&spec), Some(Status::Retrying(_))) {
                    state.status.insert(spec, Status::Retrying("waiting for the guest".into()));
                }
                continue;
            }
            match self.attach(state, info) {
                Ok(session) => {
                    tracing::info!(%spec, product = %info.product, "usb: attached");
                    state.sessions.insert(spec.clone(), session);
                    state.status.insert(spec, Status::Attached);
                }
                Err(why) => {
                    let pause = state.retry.get(&spec).map_or(FIRST_RETRY, |&(_, p)| (p * 2).min(LONGEST_RETRY));
                    state.retry.insert(spec.clone(), (now + pause, pause));
                    tracing::warn!(%spec, "usb: {why}");
                    state.status.insert(spec, Status::Retrying(why));
                }
            }
        }
    }

    fn attach(&self, state: &mut State, info: &Info) -> Result<Session, String> {
        let speed = info.speed.ok_or("the device's speed is unknown")?;
        let sink = self.server.sink();
        let device = IoUsbDevice::open(info.registry_id, sink.clone())?;
        let (guest_end, server_end) = UnixStream::pair().map_err(|e| e.to_string())?;
        let devid = state.next_devid;
        state.next_devid = state.next_devid.wrapping_add(1);
        let mut header = Vec::with_capacity(12);
        header.extend_from_slice(&devid.to_be_bytes());
        header.extend_from_slice(&(speed as u32).to_be_bytes());
        header.extend_from_slice(&info.vendor_id.to_be_bytes());
        header.extend_from_slice(&info.product_id.to_be_bytes());
        self.server.serve(&sink, server_end, Box::new(device));
        self.reactor.carry(USB_PORT, guest_end, header);
        Ok(Session { sink, registry_id: info.registry_id, started: Instant::now() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(classes: u64, device_class: u8) -> Info {
        Info {
            registry_id: 1,
            location_id: 0,
            vendor_id: 0x303a,
            product_id: 0x831a,
            bcd_device: 0,
            device_class,
            speed: Some(iousb::Speed::Full),
            interface_classes: classes,
            vendor: "Nabu Casa".into(),
            product: "ZBT_2".into(),
            serial: "E072A1D9E0CC".into(),
            driver: String::new(),
            callout: None,
        }
    }

    #[test]
    fn specs_parse_and_match() {
        let spec: Spec = "303a:831a".parse().unwrap();
        assert!(spec.matches(&info(0, 0)));
        let spec: Spec = "303a:831a:E072A1D9E0CC".parse().unwrap();
        assert!(spec.matches(&info(0, 0)));
        assert_eq!(spec.to_string(), "303a:831a:E072A1D9E0CC");
        let other: Spec = "303a:831a:OTHER".parse().unwrap();
        assert!(!other.matches(&info(0, 0)));
        assert!("zbt".parse::<Spec>().is_err());
        assert!("303a".parse::<Spec>().is_err());
        assert!("12345:1".parse::<Spec>().is_err());
    }

    #[test]
    fn serial_sticks_pass_and_the_rest_are_refused_by_default() {
        // CDC-ACM (communications, data): a Zigbee coordinator.
        assert_eq!(refusal(&info((1 << 2) | (1 << 10), 0xef)), None);
        // A vendor-class USB-to-serial bridge (CH340).
        assert_eq!(refusal(&info(1 << 63, 0xff)), None);
        assert!(refusal(&info(1 << 3, 0)).unwrap().contains("input device"));
        assert!(refusal(&info(1 << 8, 0)).unwrap().contains("storage"));
        assert!(refusal(&info(1 << 62, 0xe0)).unwrap().contains("Bluetooth"));
        assert!(refusal(&info(0, 9)).unwrap().contains("hub"));
    }
}
