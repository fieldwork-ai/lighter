//! lighter's root helper: a machine's card on the Mac's network.
//!
//! vmnet's bridged mode needs root or `com.apple.vm.networking`. Until
//! lighter holds the entitlement, this does that one thing for it, as root:
//! for a client that is lighter, run by a user allowed to, it bridges one of
//! the Mac's network cards and passes back a socket of frames (see
//! `lighter_vmnet::helper` for the conversation). Nothing else.
//!
//! Installed by `sudo lighter lan enable` as a launchd daemon whose socket
//! launchd owns, so it runs only while a machine uses it; `--socket` runs
//! it by hand for development, where `--any-client` admits unsigned builds.

use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::raw::{c_char, c_int};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lighter_vmnet::helper;

/// lighter, as Apple's notarization signs it.
const REQUIREMENT: &str = "anchor apple generic and identifier \"dev.lighter.machine\" and certificate leaf[subject.OU] = \"N7N6BNF95K\"";
/// Bridges at once, across every client.
const MOST_BRIDGES: usize = 8;
/// How long the helper stays after its last client, when launchd started it.
const IDLE_EXIT: Duration = Duration::from_secs(60);

unsafe extern "C" {
    fn lighter_peer_satisfies(
        fd: c_int,
        requirement: *const c_char,
        err: *mut c_char,
        errlen: usize,
    ) -> c_int;
    fn launch_activate_socket(
        name: *const c_char,
        fds: *mut *mut c_int,
        count: *mut usize,
    ) -> c_int;
}

struct Policy {
    uids: Vec<u32>,
    any_client: bool,
}

fn main() -> std::process::ExitCode {
    let mut socket: Option<String> = None;
    let mut uids = Vec::new();
    let mut any_client = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next(),
            "--allow-uid" => {
                if let Some(uid) = args.next().and_then(|v| v.parse().ok()) {
                    uids.push(uid);
                }
            }
            "--any-client" => any_client = true,
            "--version" => {
                println!(
                    "lighter-bridge {} (protocol {})",
                    env!("CARGO_PKG_VERSION"),
                    helper::VERSION
                );
                return std::process::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("lighter-bridge: unknown argument {other}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    // SAFETY: plain geteuid.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("lighter-bridge: must run as root (vmnet's bridged mode needs it)");
        return std::process::ExitCode::from(1);
    }
    let launched = socket.is_none();
    if launched && any_client {
        eprintln!(
            "lighter-bridge: --any-client is for a helper run by hand, never an installed one"
        );
        return std::process::ExitCode::from(2);
    }
    if any_client {
        eprintln!(
            "lighter-bridge: WARNING: admitting any client, signed or not (development only)"
        );
    }
    let listener = match &socket {
        Some(path) => {
            let _ = std::fs::remove_file(path);
            match UnixListener::bind(path) {
                Ok(l) => {
                    let c = std::ffi::CString::new(path.as_str()).expect("path");
                    // SAFETY: chmod on the path just bound.
                    unsafe { libc::chmod(c.as_ptr(), 0o666) };
                    l
                }
                Err(e) => {
                    eprintln!("lighter-bridge: cannot listen on {path}: {e}");
                    return std::process::ExitCode::from(1);
                }
            }
        }
        None => match activated() {
            Some(l) => l,
            None => {
                eprintln!("lighter-bridge: no socket from launchd (run with --socket by hand)");
                return std::process::ExitCode::from(1);
            }
        },
    };
    let policy = Arc::new(Policy { uids, any_client });
    let active = Arc::new(AtomicUsize::new(0));
    serve(listener, policy, active, launched);
    std::process::ExitCode::SUCCESS
}

/// The socket launchd made for us (`Sockets` → `Listeners` in the plist).
fn activated() -> Option<UnixListener> {
    let name = std::ffi::CString::new("Listeners").ok()?;
    let mut fds: *mut c_int = std::ptr::null_mut();
    let mut count = 0usize;
    // SAFETY: out-pointers for launchd to fill; it allocates the array.
    let r = unsafe { launch_activate_socket(name.as_ptr(), &mut fds, &mut count) };
    if r != 0 || count == 0 || fds.is_null() {
        return None;
    }
    // SAFETY: launchd gave us `count` descriptors in an array we free.
    let fd = unsafe { *fds };
    unsafe { libc::free(fds.cast()) };
    // SAFETY: a listening socket launchd handed over.
    Some(unsafe { UnixListener::from_raw_fd(fd) })
}

fn serve(listener: UnixListener, policy: Arc<Policy>, active: Arc<AtomicUsize>, launched: bool) {
    let fd = listener.as_raw_fd();
    loop {
        if launched && active.load(Ordering::Acquire) == 0 && !readable(fd, IDLE_EXIT) {
            // Idle: launchd starts us again at the next connection.
            return;
        }
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        let (policy, active) = (policy.clone(), active.clone());
        std::thread::spawn(move || {
            if let Err(why) = client(&stream, &policy, &active) {
                let _ = helper::send_line_with_fd(&stream, &format!("error {why}\n"), None);
                eprintln!("lighter-bridge: refused a client: {why}");
            }
        });
    }
}

/// Whether `fd` becomes readable within `wait`.
fn readable(fd: RawFd, wait: Duration) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd.
    unsafe { libc::poll(&mut p, 1, wait.as_millis() as c_int) > 0 }
}

/// One client: who it is, what it asks for, and its bridge until it goes.
fn client(stream: &UnixStream, policy: &Policy, active: &AtomicUsize) -> Result<(), String> {
    let (mut uid, mut gid) = (0u32, 0u32);
    // SAFETY: out-pointers for the peer's credentials.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err("no peer credentials".into());
    }
    if uid != 0 && !policy.uids.contains(&uid) {
        return Err(format!(
            "user {uid} may not use lighter's network helper (`sudo lighter lan enable` as them)"
        ));
    }
    if !policy.any_client {
        let requirement = std::ffi::CString::new(REQUIREMENT).expect("requirement");
        let mut err = [0 as c_char; 256];
        // SAFETY: a live socket, a NUL-terminated requirement and a buffer.
        let ok = unsafe {
            lighter_peer_satisfies(
                stream.as_raw_fd(),
                requirement.as_ptr(),
                err.as_mut_ptr(),
                err.len(),
            )
        };
        if ok == 0 {
            // SAFETY: NUL-terminated by the check.
            let why = unsafe { std::ffi::CStr::from_ptr(err.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(why);
        }
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let line = helper::read_hello(stream).map_err(|e| format!("no hello: {e}"))?;
    let (version, interface, mac) = helper::parse_hello(&line)?;
    if version != helper::VERSION {
        return Err(format!(
            "version {version} asked of a helper that speaks {}; run `sudo lighter lan enable` again",
            helper::VERSION
        ));
    }
    if !lighter_vmnet::interfaces().contains(&interface) {
        return Err(format!("{interface} cannot be bridged"));
    }
    if active.fetch_add(1, Ordering::AcqRel) >= MOST_BRIDGES {
        active.fetch_sub(1, Ordering::AcqRel);
        return Err(format!("{MOST_BRIDGES} bridges are already running"));
    }
    let result = (|| {
        let (bridge, near) = lighter_vmnet::Bridge::start(&interface, mac)?;
        helper::send_line_with_fd(
            stream,
            &format!("ok {} {}\n", bridge.mtu, bridge.max_packet),
            Some(near.as_raw_fd()),
        )
        .map_err(|e| e.to_string())?;
        drop(near);
        eprintln!(
            "lighter-bridge: bridged {interface} for user {uid} ({})",
            helper::mac_text(mac)
        );
        // The connection carries nothing more: its end is the bridge's.
        stream.set_read_timeout(None).map_err(|e| e.to_string())?;
        let mut byte = [0u8; 1];
        loop {
            // SAFETY: a one-byte read on a live socket.
            let n = unsafe { libc::read(stream.as_raw_fd(), byte.as_mut_ptr().cast(), 1) };
            if n <= 0 {
                break;
            }
        }
        let c = bridge.counters();
        eprintln!(
            "lighter-bridge: released {interface} for user {uid}: {} in, {} out, {} dropped",
            c.to_guest, c.to_network, c.dropped
        );
        drop(bridge);
        Ok(())
    })();
    active.fetch_sub(1, Ordering::AcqRel);
    result
}
