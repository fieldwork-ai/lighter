//! `lighter ui`: a page in the browser for what `lighter status` says.
//!
//! No app and no runtime to ship with it. The server is a blocking
//! `TcpListener` on loopback, one short-lived thread a request, and one page
//! compiled into the binary; it runs while the command does and costs nothing
//! once it is stopped. Everything it shows comes from the same calls the CLI
//! makes, and everything it does is either a Docker API call or the CLI
//! itself, run as a child, so the page can never disagree with the terminal.
//!
//! Loopback is not a boundary on its own: any site the browser has open can
//! send requests to 127.0.0.1. So the API wants a token the page was handed
//! in its URL's fragment (which is never sent to a server, and which another
//! origin cannot read), in a header another origin cannot set without a CORS
//! preflight this server never grants; and every request must name this
//! server in `Host`, which a DNS-rebinding page cannot.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::paths;

const PAGE: &str = include_str!("ui.html");

/// Requests handled at once; more are turned away rather than queued.
const MAX_IN_FLIGHT: usize = 16;
/// The request line and headers; the API takes no bodies.
const MAX_HEAD: u64 = 16 * 1024;
/// How much of the machine's log the page is shown.
const LOG_TAIL: u64 = 64 * 1024;

pub fn serve(port: u16, open: bool) -> anyhow::Result<std::process::ExitCode> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1:{port}: {e}"))?;
    let port = listener.local_addr()?.port();
    let server = Arc::new(Server {
        token: token(),
        hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
        in_flight: AtomicUsize::new(0),
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
}

struct Request {
    method: String,
    path: String,
    host: String,
    token: String,
}

struct Response {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn json(status: &'static str, value: &serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: value.to_string().into_bytes(),
        }
    }

    fn error(status: &'static str, message: impl std::fmt::Display) -> Self {
        Self::json(status, &serde_json::json!({ "error": message.to_string() }))
    }
}

impl Server {
    fn handle(&self, mut stream: TcpStream) -> std::io::Result<()> {
        // Bounds the socket I/O only; the work between (`lighter start`
        // booting the machine) takes as long as it takes.
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
             style-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'\r\n\
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
                status: "200 OK",
                content_type: "text/html; charset=utf-8",
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
        let result = match (request.method.as_str(), parts.as_slice()) {
            ("GET", ["status"]) => status(),
            ("GET", ["containers"]) => containers(),
            ("GET", ["logs"]) => logs(),
            ("POST", ["machine", action @ ("start" | "stop" | "restart")]) => machine(action),
            ("POST", ["containers", id, action @ ("start" | "stop" | "restart")]) => {
                container(id, action)
            }
            _ => return Response::error("404 Not Found", "no such endpoint"),
        };
        match result {
            Ok(value) => Response::json("200 OK", &value),
            Err(e) => Response::error("500 Internal Server Error", e),
        }
    }
}

fn read_request(stream: &TcpStream) -> anyhow::Result<Request> {
    let mut reader = BufReader::new(stream.take(MAX_HEAD));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut words = line.split_whitespace();
    let (Some(method), Some(target)) = (words.next(), words.next()) else {
        anyhow::bail!("malformed request line");
    };
    let path = target.split('?').next().unwrap_or(target).to_string();
    let mut request = Request {
        method: method.to_string(),
        path,
        host: String::new(),
        token: String::new(),
    };
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            anyhow::bail!("headers ended early");
        }
        let header = header.trim_end();
        if header.is_empty() {
            return Ok(request);
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        match name.to_ascii_lowercase().as_str() {
            "host" => request.host = value,
            "x-lighter-token" => request.token = value,
            _ => {}
        }
    }
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

fn containers() -> anyhow::Result<serde_json::Value> {
    let socket = paths::docker_socket()?;
    Ok(lighter_docker::http::get_json(
        &socket,
        "/containers/json?all=1",
    )?)
}

fn container(id: &str, action: &str) -> anyhow::Result<serde_json::Value> {
    // An ID or a name, as Docker allows them; nothing that could reshape the
    // path it is put into.
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        anyhow::bail!("not a container ID or name: {id:?}");
    }
    let socket = paths::docker_socket()?;
    let code = lighter_docker::http::post(&socket, &format!("/containers/{id}/{action}"))?;
    match code {
        // 304: already in the state asked for.
        204 | 304 => Ok(serde_json::json!({ "ok": true })),
        404 => anyhow::bail!("no such container: {id}"),
        code => anyhow::bail!("docker answered {code} to {action} {id}"),
    }
}

/// Runs this binary's own subcommand, so the page does exactly what typing
/// `lighter start` would, Docker context included.
fn machine(action: &str) -> anyhow::Result<serde_json::Value> {
    let output = std::process::Command::new(paths::executable()?)
        .arg(action)
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
        }
    }

    fn request(method: &str, path: &str, host: &str, token: &str) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            host: host.into(),
            token: token.into(),
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
    }

    #[test]
    fn a_container_id_cannot_reshape_the_path() {
        assert!(container("../../images/x", "start").is_err());
        assert!(container("abc?force=1", "stop").is_err());
    }

    #[test]
    fn tokens_compare_whole() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("ab", "abc"));
        assert_eq!(token().len(), 32);
    }
}
