//! `lighter ui`: a page in the browser for the machine and what runs on it.
//!
//! No app and no runtime to ship with it. The server is a blocking
//! `TcpListener` on loopback, one short-lived thread a request, and one page
//! compiled into the binary; it runs while the command does and costs nothing
//! once it is stopped. The page does the work: the server answers three kinds
//! of question — the machine's own (status, log, config), which it answers
//! with the CLI's calls or by running the CLI itself; and Docker's, which it
//! relays to the Docker socket unchanged, for an allow-list of endpoints that
//! read, start, stop and delete, and none that create a container; and a
//! terminal's, a shell in a running container (unless `--no-exec`). So the page
//! can never disagree with the terminal, and cannot do through this server
//! what `docker run --privileged` would.
//!
//! A terminal needs bytes both ways for as long as the shell lives, which is
//! what WebSockets are for; long polling does it with the HTTP this server
//! already speaks. Docker hands over the exec's connection, a thread reads it
//! into a buffer, and the page holds one request open that returns as soon as
//! there is output; keystrokes go up in short POSTs. A terminal nobody has
//! asked about for a minute (a closed tab) is hung up on.
//!
//! Loopback is not a boundary on its own: any site the browser has open can
//! send requests to 127.0.0.1. So the API wants a token the page was handed
//! in its URL's fragment (which is never sent to a server, and which another
//! origin cannot read), in a header another origin cannot set without a CORS
//! preflight this server never grants; and every request must name this
//! server in `Host`, which a DNS-rebinding page cannot.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::paths;

const PAGE: &str = include_str!("ui.html");

/// Requests handled at once; more are turned away rather than queued.
const MAX_IN_FLIGHT: usize = 32;
/// The request line and headers.
const MAX_HEAD: u64 = 16 * 1024;
/// A request's body: only settings are sent, and they are small.
const MAX_BODY: usize = 16 * 1024;
/// How much of the machine's log the page is shown.
const LOG_TAIL: u64 = 64 * 1024;
/// A Docker call; a container given ten seconds to stop fits well inside.
const DOCKER_TIMEOUT: Duration = Duration::from_secs(60);
/// Pulling an image, which is a download.
const PULL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Terminals open at once.
const MAX_TERMINALS: usize = 8;
/// How long a terminal's read waits for output before answering empty.
const POLL_WAIT: Duration = Duration::from_secs(20);
/// A terminal not read for this long has no page left to show it.
const TERMINAL_IDLE: Duration = Duration::from_secs(60);
/// Output kept for a page that has fallen behind; the oldest goes first.
const MAX_PENDING: usize = 1 << 20;

pub fn serve(port: u16, open: bool, exec: bool) -> anyhow::Result<std::process::ExitCode> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1:{port}: {e}"))?;
    let port = listener.local_addr()?.port();
    let server = Arc::new(Server {
        token: token(),
        hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
        in_flight: AtomicUsize::new(0),
        exec,
        terminals: Mutex::default(),
    });
    let reaper = Arc::clone(&server);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(TERMINAL_IDLE / 4);
            reaper.reap();
        }
    });
    let url = format!("http://127.0.0.1:{port}/#{}", server.token);
    outln!("lighter ui at {url}");
    outln!("Press Ctrl-C to stop it.");
    if open
        && let Err(e) = std::process::Command::new("/usr/bin/open")
            .arg(&url)
            .status()
    {
        eprintln!("lighter: could not open a browser: {e}");
    }
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                tracing::debug!("ui: accept failed: {e}");
                continue;
            }
        };
        if server.in_flight.fetch_add(1, Ordering::AcqRel) >= MAX_IN_FLIGHT {
            server.in_flight.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            if let Err(e) = server.handle(stream) {
                tracing::debug!("ui: {e}");
            }
            server.in_flight.fetch_sub(1, Ordering::AcqRel);
        });
    }
    Ok(std::process::ExitCode::SUCCESS)
}

struct Server {
    token: String,
    /// The `Host` values this server answers to.
    hosts: [String; 2],
    in_flight: AtomicUsize,
    /// Whether the page may open a shell in a container.
    exec: bool,
    terminals: Mutex<HashMap<String, Arc<Terminal>>>,
}

/// A shell in a container, and what it has said that the page has not read.
struct Terminal {
    exec_id: String,
    container: String,
    /// In the environment of the shell and everything it starts, so a hang-up
    /// can find them all.
    marker: String,
    input: Mutex<UnixStream>,
    output: Mutex<Output>,
    ready: Condvar,
    seen: Mutex<Instant>,
}

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    ended: bool,
}

/// A lock whose holder panicked still guards usable data here: a buffer of
/// bytes or a socket.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Terminal {
    /// Copies the shell's output into the buffer until it ends.
    fn pump(&self, mut reader: impl Read) {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut out = lock(&self.output);
                    out.bytes.extend_from_slice(&buf[..n]);
                    if out.bytes.len() > MAX_PENDING {
                        let excess = out.bytes.len() - MAX_PENDING;
                        out.bytes.drain(..excess);
                    }
                    self.ready.notify_all();
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        lock(&self.output).ended = true;
        self.ready.notify_all();
    }

    /// Closes the connection: the pump sees the end.
    #[cfg(test)]
    fn close(&self) {
        let _ = lock(&self.input).shutdown(Shutdown::Both);
    }

    /// Hangs up as closing a terminal window does: SIGHUP to the shell and
    /// to what it started, which Docker does not send when the connection
    /// goes (the shell would otherwise live on in the container); then the
    /// connection. On a thread, as it is a Docker call or two.
    fn hang_up(&self) {
        let container = self.container.clone();
        let marker = self.marker.clone();
        let input = lock(&self.input).try_clone();
        std::thread::spawn(move || {
            if let Err(e) = signal_terminal(&container, &marker) {
                tracing::debug!("ui: hanging up a terminal: {e}");
            }
            if let Ok(input) = input {
                let _ = input.shutdown(Shutdown::Both);
            }
        });
    }
}

/// Sends SIGHUP to every process in the container that carries `marker`.
fn signal_terminal(container: &str, marker: &str) -> anyhow::Result<()> {
    // The marker is hex from `token`, so it needs no quoting.
    let script = format!(
        r#"for p in /proc/[0-9]*; do if tr '\0' '\n' < "$p/environ" 2>/dev/null | grep -qx 'LIGHTER_TERMINAL={marker}'; then kill -HUP "${{p#/proc/}}" 2>/dev/null; fi; done"#
    );
    let socket = paths::docker_socket()?;
    let create = serde_json::json!({ "Cmd": ["/bin/sh", "-c", script] });
    let created = lighter_docker::http::request(
        &socket,
        "POST",
        &format!("/containers/{container}/exec"),
        Some(create.to_string().as_bytes()),
        DOCKER_TIMEOUT,
    )?;
    let created: serde_json::Value = serde_json::from_slice(&created.body)?;
    let Some(id) = created.get("Id").and_then(|id| id.as_str()) else {
        anyhow::bail!("docker did not create the exec: {created}");
    };
    let started = lighter_docker::http::request(
        &socket,
        "POST",
        &format!("/exec/{id}/start"),
        Some(br#"{"Detach":true}"#),
        DOCKER_TIMEOUT,
    )?;
    if started.status >= 300 {
        anyhow::bail!("docker answered {} to starting it", started.status);
    }
    Ok(())
}

struct Request {
    method: String,
    path: String,
    query: String,
    host: String,
    token: String,
    body: Vec<u8>,
}

struct Response {
    status: String,
    content_type: String,
    body: Vec<u8>,
}

impl Response {
    fn json(status: &str, value: &serde_json::Value) -> Self {
        Self {
            status: status.to_string(),
            content_type: "application/json".to_string(),
            body: value.to_string().into_bytes(),
        }
    }

    fn error(status: &str, message: impl std::fmt::Display) -> Self {
        Self::json(status, &serde_json::json!({ "error": message.to_string() }))
    }
}

impl Server {
    fn handle(&self, mut stream: TcpStream) -> std::io::Result<()> {
        // Bounds the socket I/O only; the work between (`lighter start`
        // booting the machine, an image pull) takes as long as it takes.
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let response = match read_request(&stream) {
            Ok(request) => self.route(&request),
            Err(e) => Response::error("400 Bad Request", e),
        };
        write!(
            stream,
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
             Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; \
             style-src 'unsafe-inline'; connect-src 'self'; img-src data:; \
             frame-ancestors 'none'\r\n\
             Connection: close\r\n\r\n",
            response.status,
            response.content_type,
            response.body.len()
        )?;
        stream.write_all(&response.body)?;
        stream.flush()
    }

    fn route(&self, request: &Request) -> Response {
        if !self.hosts.contains(&request.host) {
            return Response::error("421 Misdirected Request", "unexpected Host");
        }
        if request.method == "GET" && request.path == "/" {
            return Response {
                status: "200 OK".to_string(),
                content_type: "text/html; charset=utf-8".to_string(),
                body: PAGE.as_bytes().to_vec(),
            };
        }
        if !request.path.starts_with("/api/") {
            return Response::error("404 Not Found", "no such page");
        }
        if !same(&request.token, &self.token) {
            return Response::error("403 Forbidden", "missing or wrong token");
        }
        let parts: Vec<&str> = request
            .path
            .trim_start_matches("/api/")
            .split('/')
            .collect();
        if let ["exec", rest @ ..] = parts.as_slice() {
            return self.terminal(&request.method, rest, request);
        }
        let result = match (request.method.as_str(), parts.as_slice()) {
            ("GET", ["status"]) => status().map(|mut status| {
                status["exec"] = self.exec.into();
                status
            }),
            ("GET", ["logs"]) => logs(),
            ("GET", ["config"]) => config(),
            ("POST", ["config"]) => configure(&request.body),
            ("POST", ["machine", action @ ("start" | "stop" | "restart")]) => {
                run_self(&[action.to_string()])
            }
            (method, ["docker", rest @ ..]) => return docker(method, rest, &request.query),
            _ => return Response::error("404 Not Found", "no such endpoint"),
        };
        match result {
            Ok(value) => Response::json("200 OK", &value),
            Err(e) => Response::error("500 Internal Server Error", e),
        }
    }

    fn terminal(&self, method: &str, parts: &[&str], request: &Request) -> Response {
        if !self.exec {
            return Response::error("403 Forbidden", "terminals are off (lighter ui --no-exec)");
        }
        if let ("POST", []) = (method, parts) {
            return match self.open_terminal(&request.body) {
                Ok(id) => Response::json("200 OK", &serde_json::json!({ "session": id })),
                Err(e) => Response::error("500 Internal Server Error", e),
            };
        }
        let Some((id, rest)) = parts.split_first() else {
            return Response::error("404 Not Found", "no such endpoint");
        };
        let Some(terminal) = lock(&self.terminals).get(*id).cloned() else {
            return Response::error("410 Gone", "this terminal has closed");
        };
        *lock(&terminal.seen) = Instant::now();
        match (method, rest) {
            ("GET", []) => self.read_terminal(id, &terminal),
            ("POST", []) => match lock(&terminal.input).write_all(&request.body) {
                Ok(()) => Response::json("200 OK", &serde_json::json!({})),
                Err(e) => Response::error("410 Gone", e),
            },
            ("POST", ["resize"]) => match resize(&terminal.exec_id, &request.query) {
                Ok(()) => Response::json("200 OK", &serde_json::json!({})),
                Err(e) => Response::error("500 Internal Server Error", e),
            },
            ("DELETE", []) => {
                if let Some(terminal) = lock(&self.terminals).remove(*id) {
                    terminal.hang_up();
                }
                Response::json("200 OK", &serde_json::json!({}))
            }
            _ => Response::error("404 Not Found", "no such endpoint"),
        }
    }

    /// Starts a shell in a container with a terminal of the page's size, and
    /// returns the name the page reads and writes it by.
    fn open_terminal(&self, body: &[u8]) -> anyhow::Result<String> {
        #[derive(serde::Deserialize)]
        struct Open {
            container: String,
            cols: u16,
            rows: u16,
            #[serde(default)]
            shell: String,
        }
        let open: Open = serde_json::from_slice(body)?;
        if !is_name(&open.container) {
            anyhow::bail!("not a container ID or name: {:?}", open.container);
        }
        let cmd: &[&str] = match open.shell.as_str() {
            "" | "auto" => &[
                "/bin/sh",
                "-c",
                "if command -v bash >/dev/null 2>&1; then exec bash; else exec sh; fi",
            ],
            "bash" => &["bash"],
            "sh" => &["sh"],
            other => anyhow::bail!("not a shell this page starts: {other:?}"),
        };
        let (cols, rows) = (open.cols.clamp(10, 500), open.rows.clamp(4, 300));
        self.reap();
        if lock(&self.terminals).len() >= MAX_TERMINALS {
            anyhow::bail!("{MAX_TERMINALS} terminals are open already; close one first");
        }

        let marker = token();
        let socket = paths::docker_socket()?;
        let create = serde_json::json!({
            "AttachStdin": true,
            "AttachStdout": true,
            "AttachStderr": true,
            "Tty": true,
            "Env": ["TERM=xterm-256color", format!("LIGHTER_TERMINAL={marker}")],
            "Cmd": cmd,
        });
        let created = lighter_docker::http::request(
            &socket,
            "POST",
            &format!("/containers/{}/exec", open.container),
            Some(create.to_string().as_bytes()),
            DOCKER_TIMEOUT,
        )?;
        let created: serde_json::Value = serde_json::from_slice(&created.body)?;
        let Some(exec_id) = created.get("Id").and_then(|id| id.as_str()) else {
            anyhow::bail!(
                "{}",
                created
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("docker did not create the exec")
            );
        };
        let start =
            serde_json::json!({ "Detach": false, "Tty": true, "ConsoleSize": [rows, cols] });
        let (reader, writer) = lighter_docker::http::hijack(
            &socket,
            &format!("/exec/{exec_id}/start"),
            start.to_string().as_bytes(),
        )?;
        let terminal = Arc::new(Terminal {
            exec_id: exec_id.to_string(),
            container: open.container.clone(),
            marker,
            input: Mutex::new(writer),
            output: Mutex::default(),
            ready: Condvar::new(),
            seen: Mutex::new(Instant::now()),
        });
        let pump = Arc::clone(&terminal);
        std::thread::spawn(move || pump.pump(reader));
        let id = token();
        lock(&self.terminals).insert(id.clone(), terminal);
        Ok(id)
    }

    /// Answers with the output so far, waiting for some if there is none;
    /// 204 if none came, and 410 with the exit code once the shell is gone.
    fn read_terminal(&self, id: &str, terminal: &Terminal) -> Response {
        let (mut out, _) = terminal
            .ready
            .wait_timeout_while(lock(&terminal.output), POLL_WAIT, |out| {
                out.bytes.is_empty() && !out.ended
            })
            .unwrap_or_else(PoisonError::into_inner);
        *lock(&terminal.seen) = Instant::now();
        if !out.bytes.is_empty() {
            return Response {
                status: "200 OK".to_string(),
                content_type: "application/octet-stream".to_string(),
                body: std::mem::take(&mut out.bytes),
            };
        }
        if !out.ended {
            return Response {
                status: "204 No Content".to_string(),
                content_type: "application/octet-stream".to_string(),
                body: Vec::new(),
            };
        }
        drop(out);
        lock(&self.terminals).remove(id);
        let code = paths::docker_socket()
            .ok()
            .and_then(|socket| {
                lighter_docker::http::get_json(&socket, &format!("/exec/{}/json", terminal.exec_id))
                    .ok()
            })
            .and_then(|exec| exec.get("ExitCode")?.as_i64());
        Response::json("410 Gone", &serde_json::json!({ "exit_code": code }))
    }

    /// Hangs up on terminals no page is reading.
    fn reap(&self) {
        lock(&self.terminals).retain(|_, terminal| {
            let alive = lock(&terminal.seen).elapsed() < TERMINAL_IDLE;
            if !alive {
                terminal.hang_up();
            }
            alive
        });
    }
}

/// Tells the shell its terminal changed size (`cols=120&rows=40`).
fn resize(exec_id: &str, query: &str) -> anyhow::Result<()> {
    let mut size = (None, None);
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("cols", v)) => size.0 = v.parse::<u16>().ok(),
            Some(("rows", v)) => size.1 = v.parse::<u16>().ok(),
            _ => {}
        }
    }
    let (Some(cols), Some(rows)) = size else {
        anyhow::bail!("a resize needs cols and rows");
    };
    let socket = paths::docker_socket()?;
    let reply = lighter_docker::http::request(
        &socket,
        "POST",
        &format!("/exec/{exec_id}/resize?h={rows}&w={cols}"),
        None,
        DOCKER_TIMEOUT,
    )?;
    if reply.status >= 300 {
        anyhow::bail!("docker answered {} to a resize", reply.status);
    }
    Ok(())
}

fn read_request(stream: &TcpStream) -> anyhow::Result<Request> {
    let mut reader = BufReader::new(stream.take(MAX_HEAD + MAX_BODY as u64));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut words = line.split_whitespace();
    let (Some(method), Some(target)) = (words.next(), words.next()) else {
        anyhow::bail!("malformed request line");
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut request = Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        host: String::new(),
        token: String::new(),
        body: Vec::new(),
    };
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            anyhow::bail!("headers ended early");
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        match name.to_ascii_lowercase().as_str() {
            "host" => request.host = value,
            "x-lighter-token" => request.token = value,
            "content-length" => length = value.parse()?,
            _ => {}
        }
    }
    if length > MAX_BODY {
        anyhow::bail!("a body of {length} bytes is more than this server takes");
    }
    request.body.resize(length, 0);
    reader.read_exact(&mut request.body)?;
    Ok(request)
}

/// Compares in time independent of where the strings first differ.
fn same(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn token() -> String {
    let mut bytes = [0u8; 16];
    // SAFETY: arc4random_buf fills the buffer it is given.
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// An ID or a name as Docker allows them (`sha256:…` included), and nothing
/// that could reshape the path it is put into.
fn is_name(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b':'))
}

/// The Docker endpoints the page may reach. Reading anything, and starting,
/// stopping, pulling, pruning and deleting; never creating a container or
/// executing in one, which is where Docker's power over the machine lies.
fn allowed(method: &str, parts: &[&str]) -> bool {
    if !parts.iter().all(|p| is_name(p)) {
        return false;
    }
    matches!(
        (method, parts),
        (
            "GET",
            ["containers" | "images", "json"]
                | ["volumes" | "networks" | "info" | "version"]
                | ["system", "df"]
                | ["containers", _, "json" | "logs" | "stats" | "top"]
                | ["images", _, "json" | "history"]
                | ["volumes" | "networks", _]
        ) | (
            "POST",
            [
                "containers",
                _,
                "start" | "stop" | "restart" | "pause" | "unpause" | "kill"
            ] | ["images", "create"]
                | [
                    "containers" | "images" | "volumes" | "networks" | "build",
                    "prune"
                ]
        ) | (
            "DELETE",
            ["containers" | "images" | "volumes" | "networks", _]
        )
    )
}

/// A query string passed to Docker as given: its parameters are Docker's to
/// judge, and these characters cannot end it or start another request line.
fn is_query(query: &str) -> bool {
    query.len() <= 2048
        && query.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'=' | b'&' | b'_' | b'.' | b'-' | b':' | b'%' | b',' | b'+' | b'/'
                )
        })
}

fn docker(method: &str, parts: &[&str], query: &str) -> Response {
    if !allowed(method, parts) || !is_query(query) {
        return Response::error("403 Forbidden", "not an endpoint this page may use");
    }
    // A response is read whole before it is relayed, so nothing may be asked
    // to stream: stats would never end, nor would a followed log.
    let query = match parts.last() {
        Some(&"stats") => "stream=false",
        Some(&"logs") if query.contains("follow") => {
            return Response::error("403 Forbidden", "logs are polled here, not followed");
        }
        _ => query,
    };
    let mut path = format!("/{}", parts.join("/"));
    if !query.is_empty() {
        path.push('?');
        path.push_str(query);
    }
    let timeout = if parts == ["images", "create"] {
        PULL_TIMEOUT
    } else {
        DOCKER_TIMEOUT
    };
    let socket = match paths::docker_socket() {
        Ok(socket) => socket,
        Err(e) => return Response::error("500 Internal Server Error", e),
    };
    match lighter_docker::http::request(&socket, method, &path, None, timeout) {
        Ok(reply) => Response {
            status: format!("{} Docker", reply.status),
            content_type: if reply.content_type.is_empty() {
                "application/octet-stream".to_string()
            } else {
                reply.content_type
            },
            body: reply.body,
        },
        Err(e) => Response::error("502 Bad Gateway", e),
    }
}

fn status() -> anyhow::Result<serde_json::Value> {
    let status = crate::machine::status()?;
    let config = crate::config::Config::load()?;
    Ok(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "running": status.running,
        "pid": status.pid,
        "docker": status.docker,
        "footprint_mib": status.footprint_mib,
        "guest_memory": status.guest_memory.map(|m| serde_json::json!({
            "total_mib": m.total_mib,
            "available_mib": m.available_mib,
        })),
        "cpus": config.vcpus(),
        "memory_mib": config.memory_mib(),
        "disk_gib": config.disk_gib,
        "socket": paths::docker_socket()?.display().to_string(),
    }))
}

fn config() -> anyhow::Result<serde_json::Value> {
    let config = crate::config::Config::load()?;
    Ok(serde_json::json!({
        "config": config,
        "cpus": config.vcpus(),
        "memory_mib": config.memory_mib(),
    }))
}

/// Changes settings by running `lighter config` with them, so the page is
/// held to the same checks the command line is.
fn configure(body: &[u8]) -> anyhow::Result<serde_json::Value> {
    let given: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(body)?;
    let mut args = vec!["config".to_string()];
    for (key, value) in &given {
        let (flag, arg) = match (key.as_str(), value) {
            ("cpus" | "memory" | "disk", serde_json::Value::Number(n)) => {
                let n = n
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("{key} must be a whole number"))?;
                (key.as_str(), n.to_string())
            }
            ("gpu" | "ane" | "mps" | "metal" | "video", serde_json::Value::Bool(on)) => {
                (key.as_str(), if *on { "on" } else { "off" }.to_string())
            }
            ("publish", serde_json::Value::String(s))
                if matches!(s.as_str(), "lan" | "localhost") =>
            {
                ("publish", s.clone())
            }
            ("resources", serde_json::Value::String(s))
                if matches!(s.as_str(), "fixed" | "cooperative") =>
            {
                ("resources", s.clone())
            }
            _ => anyhow::bail!("cannot set {key} to {value}"),
        };
        args.push(format!("--{flag}"));
        args.push(arg);
    }
    if args.len() == 1 {
        anyhow::bail!("nothing to change");
    }
    run_self(&args)
}

/// Runs this binary's own subcommand, so the page does exactly what typing
/// `lighter start` would, Docker context included.
fn run_self(args: &[String]) -> anyhow::Result<serde_json::Value> {
    let output = std::process::Command::new(paths::executable()?)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(serde_json::json!({ "ok": output.status.success(), "output": text }))
}

fn logs() -> anyhow::Result<serde_json::Value> {
    let path = paths::log_file()?;
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::json!({ "text": "" }));
        }
        Err(e) => return Err(e.into()),
    };
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    // Start at a whole line when the tail cut one.
    let text = if len > LOG_TAIL {
        text.split_once('\n').map_or(&*text, |(_, rest)| rest)
    } else {
        &text
    };
    Ok(serde_json::json!({ "text": text }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server {
            token: "secret".into(),
            hosts: ["127.0.0.1:1".into(), "localhost:1".into()],
            in_flight: AtomicUsize::new(0),
            exec: true,
            terminals: Mutex::default(),
        }
    }

    fn request(method: &str, path: &str, host: &str, token: &str) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            query: String::new(),
            host: host.into(),
            token: token.into(),
            body: Vec::new(),
        }
    }

    #[test]
    fn a_foreign_host_is_refused() {
        let response = server().route(&request("GET", "/", "evil.example:1", ""));
        assert!(response.status.starts_with("421"));
    }

    #[test]
    fn the_page_needs_no_token_but_the_api_does() {
        let s = server();
        assert!(
            s.route(&request("GET", "/", "localhost:1", ""))
                .status
                .starts_with("200")
        );
        let api = s.route(&request("GET", "/api/status", "localhost:1", "wrong!"));
        assert!(api.status.starts_with("403"));
        let docker = s.route(&request("GET", "/api/docker/info", "localhost:1", ""));
        assert!(docker.status.starts_with("403"));
    }

    #[test]
    fn only_listed_docker_endpoints_are_relayed() {
        for (method, path) in [
            ("GET", "containers/json"),
            ("GET", "containers/abc123/logs"),
            ("GET", "system/df"),
            ("POST", "containers/web-1/stop"),
            ("POST", "images/create"),
            ("POST", "build/prune"),
            ("DELETE", "images/sha256:0123abcd"),
            ("DELETE", "volumes/my_data"),
        ] {
            let parts: Vec<&str> = path.split('/').collect();
            assert!(allowed(method, &parts), "{method} {path}");
        }
        for (method, path) in [
            ("POST", "containers/create"),
            ("POST", "containers/abc/exec"),
            ("POST", "containers/abc/attach"),
            ("GET", "containers/../images/json"),
            ("DELETE", "containers/a/b"),
            ("PUT", "containers/abc/archive"),
            ("POST", "swarm/init"),
        ] {
            let parts: Vec<&str> = path.split('/').collect();
            assert!(!allowed(method, &parts), "{method} {path}");
        }
    }

    #[test]
    fn a_query_cannot_end_the_request_line() {
        assert!(is_query(
            "all=1&filters=%7B%22dangling%22%3A%5B%22true%22%5D%7D"
        ));
        assert!(is_query("fromImage=library/nginx&tag=alpine"));
        assert!(!is_query("all=1 HTTP/1.1\r\nX: y"));
        assert!(!is_query("a=#b"));
    }

    #[test]
    fn settings_are_checked_before_the_cli_sees_them() {
        assert!(configure(br#"{"cpus":"4; rm"}"#).is_err());
        assert!(configure(br#"{"publish":"everywhere"}"#).is_err());
        assert!(configure(br#"{"share":"/"}"#).is_err());
        assert!(configure(b"{}").is_err());
    }

    #[test]
    fn terminals_can_be_turned_off_and_are_asked_for_carefully() {
        let mut s = server();
        let mut open = request("POST", "/api/exec", "localhost:1", "secret");
        open.body = br#"{"container":"web","cols":80,"rows":24,"shell":"rm -rf /"}"#.to_vec();
        let refused = s.route(&open);
        assert!(refused.status.starts_with("500"));
        assert!(String::from_utf8_lossy(&refused.body).contains("not a shell"));
        open.body = br#"{"container":"../x","cols":80,"rows":24}"#.to_vec();
        assert!(s.route(&open).status.starts_with("500"));
        let gone = s.route(&request("GET", "/api/exec/nope", "localhost:1", "secret"));
        assert!(gone.status.starts_with("410"));
        s.exec = false;
        assert!(s.route(&open).status.starts_with("403"));
    }

    #[test]
    fn a_terminal_hands_over_its_output_then_its_end() {
        // `ours` is the page's end of the exec's connection; `shell` the shell's.
        let (ours, shell) = UnixStream::pair().unwrap();
        let reader = ours.try_clone().unwrap();
        let terminal = Arc::new(Terminal {
            exec_id: "x".into(),
            container: "c".into(),
            marker: "m".into(),
            input: Mutex::new(ours),
            output: Mutex::default(),
            ready: Condvar::new(),
            seen: Mutex::new(Instant::now()),
        });
        let pump = Arc::clone(&terminal);
        let task = std::thread::spawn(move || pump.pump(reader));
        (&shell).write_all(b"$ ").unwrap();
        let s = server();
        let first = s.read_terminal("t", &terminal);
        assert_eq!(
            (first.status.as_str(), first.body.as_slice()),
            ("200 OK", &b"$ "[..])
        );
        terminal.close();
        task.join().unwrap();
        assert!(s.read_terminal("t", &terminal).status.starts_with("410"));
    }

    #[test]
    fn tokens_compare_whole() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("ab", "abc"));
        assert_eq!(token().len(), 32);
    }
}
