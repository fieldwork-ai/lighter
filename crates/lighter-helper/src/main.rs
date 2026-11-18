//! lighter's root helper: the sockets macOS keeps from a process that is
//! not root.
//!
//! For a client that is lighter, run by a user allowed to, it binds a port
//! below 1024 on one of the Mac's addresses and passes the socket back
//! (see the library for why, and for the conversation). Nothing else: the
//! socket is the client's from then on, and the helper holds nothing.
//!
//! Installed by `sudo lighter helper install` as a launchd daemon whose
//! socket launchd owns, so it runs only while asked; `--socket` runs it by
//! hand for development, where `--any-client` admits unsigned builds.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::raw::{c_char, c_int};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::time::Duration;

use lighter_helper::Request;

/// lighter's machine, as Apple's notarization signs it: the process that
/// publishes ports.
const REQUIREMENT: &str = "anchor apple generic and identifier \"dev.lighter.machine\" and certificate leaf[subject.OU] = \"N7N6BNF95K\"";
/// How long the helper stays after its last request, when launchd started it.
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
                    "lighter-helper {} (protocol {})",
                    env!("CARGO_PKG_VERSION"),
                    lighter_helper::VERSION
                );
                return std::process::ExitCode::SUCCESS;
            }
            other => {
                eprintln!("lighter-helper: unknown argument {other}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    // SAFETY: plain geteuid.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("lighter-helper: must run as root (ports below 1024 need it)");
        return std::process::ExitCode::from(1);
    }
    let launched = socket.is_none();
    if launched && any_client {
        eprintln!(
            "lighter-helper: --any-client is for a helper run by hand, never an installed one"
        );
        return std::process::ExitCode::from(2);
    }
    if any_client {
        eprintln!(
            "lighter-helper: WARNING: admitting any client, signed or not (development only)"
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
                    eprintln!("lighter-helper: cannot listen on {path}: {e}");
                    return std::process::ExitCode::from(1);
                }
            }
        }
        None => match activated() {
            Some(l) => l,
            None => {
                eprintln!("lighter-helper: no socket from launchd (run with --socket by hand)");
                return std::process::ExitCode::from(1);
            }
        },
    };
    serve(listener, Arc::new(Policy { uids, any_client }), launched);
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

/// Answers clients one at a time: each is a bind, done in microseconds,
/// and nothing is held between them.
fn serve(listener: UnixListener, policy: Arc<Policy>, launched: bool) {
    let fd = listener.as_raw_fd();
    loop {
        if launched && !readable(fd, IDLE_EXIT) {
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
        match client(&stream, &policy) {
            Ok(done) => eprintln!("lighter-helper: {done}"),
            Err(why) => {
                let _ = lighter_helper::send_line_with_fd(
                    &stream,
                    &lighter_helper::error_line(&why),
                    None,
                );
                eprintln!("lighter-helper: refused a client: {why}");
            }
        }
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

/// One client: who it is, what it asks for, and the answer.
fn client(stream: &UnixStream, policy: &Policy) -> Result<String, io::Error> {
    let (mut uid, mut gid) = (0u32, 0u32);
    // SAFETY: out-pointers for the peer's credentials.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::other("no peer credentials"));
    }
    if uid != 0 && !policy.uids.contains(&uid) {
        return Err(io::Error::other(format!(
            "user {uid} may not use lighter's helper (`sudo lighter helper install` as them)"
        )));
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
            return Err(io::Error::other(why));
        }
    }
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let line = lighter_helper::read_hello(stream)
        .map_err(|e| io::Error::other(format!("no hello: {e}")))?;
    let (version, request) = lighter_helper::parse_hello(&line).map_err(io::Error::other)?;
    if version != lighter_helper::VERSION {
        return Err(io::Error::other(format!(
            "version {version} asked of a helper that speaks {}; run `sudo lighter helper install` again",
            lighter_helper::VERSION
        )));
    }
    match request {
        Request::Bind(kind, addr) => {
            lighter_helper::permitted(addr).map_err(io::Error::other)?;
            // The OS's error as it is, errno and all: the client tells a
            // port held elsewhere by it.
            let socket = lighter_helper::bind_socket_as_root(addr, kind)?;
            lighter_helper::send_line_with_fd(stream, "ok\n", Some(socket.as_raw_fd()))?;
            Ok(format!("bound {kind:?} {addr} for user {uid}"))
        }
    }
}
