//! Just enough HTTP to talk to the Docker API over a unix socket.
//!
//! Not a general client, and not trying to be. The whole vocabulary is two
//! request shapes against a socket on this machine: fetch a JSON document, and
//! read an endless stream of JSON events. A real client crate would bring an
//! async runtime and a TLS stack to a conversation that needs neither.

use std::io;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A way to end a stream from another thread.
///
/// The connection is shut down under the reader, which sees end of file and
/// returns; a caller that loops on reconnecting asks [`Stop::asked`] first.
/// Needed because dockerd, asked to stop, gives an active connection five
/// seconds of grace before it will exit, and the event stream is exactly
/// that connection.
#[derive(Default)]
pub struct Stop {
    // Own the descriptor rather than remembering a raw fd which another
    // thread could close and reuse before shutdown runs.
    stream: Mutex<Option<UnixStream>>,
    asked: AtomicBool,
}

impl Stop {
    pub fn new() -> std::sync::Arc<Stop> {
        std::sync::Arc::new(Self::default())
    }

    pub fn asked(&self) -> bool {
        self.asked.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        self.asked.store(true, Ordering::SeqCst);
        if let Some(stream) = self.stream.lock().expect("stop poisoned").as_ref() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    fn attach(&self, stream: &UnixStream) -> io::Result<()> {
        let mut active = self.stream.lock().expect("stop poisoned");
        if self.asked() {
            return Err(cancelled());
        }
        *active = Some(stream.try_clone()?);
        Ok(())
    }

    fn detach(&self) {
        self.stream.lock().expect("stop poisoned").take();
    }
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "request cancelled")
}
fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "request deadline elapsed")
}

/// Adapts a nonblocking socket to Read/Write with one absolute deadline.
/// Every syscall checks the remaining budget; trickled bytes cannot restart
/// the clock. Only an established event stream clears its deadline.
struct Connection<'a> {
    stream: UnixStream,
    deadline: Option<Instant>,
    stop: Option<&'a Stop>,
}

impl<'a> Connection<'a> {
    fn connect(path: &Path, deadline: Instant, stop: Option<&'a Stop>) -> io::Result<Self> {
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        let bytes = path.as_os_str().as_bytes();
        if bytes.contains(&0) || bytes.len() >= addr.sun_path.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid unix socket path",
            ));
        }
        addr.sun_family = libc::AF_UNIX as _;
        addr.sun_len = std::mem::size_of_val(&addr) as u8;
        for (out, &byte) in addr.sun_path.iter_mut().zip(bytes) {
            *out = byte as _;
        }
        // SAFETY: a plain socket allocation, checked before ownership is taken.
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        stream.set_nonblocking(true)?;
        // SAFETY: a live descriptor; ensure it is not inherited by exec.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let connection = Self {
            stream,
            deadline: Some(deadline),
            stop,
        };
        connection.check()?;
        if let Some(stop) = stop {
            stop.attach(&connection.stream)?;
        }
        // SAFETY: a correctly sized sockaddr_un that lives through connect.
        if unsafe {
            libc::connect(
                fd,
                (&addr as *const libc::sockaddr_un).cast(),
                std::mem::size_of_val(&addr) as _,
            )
        } < 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(error);
            }
            connection.wait(libc::POLLOUT)?;
            if let Some(error) = connection.stream.take_error()? {
                return Err(error);
            }
        }
        Ok(connection)
    }

    fn check(&self) -> io::Result<()> {
        if self.stop.is_some_and(Stop::asked) {
            return Err(cancelled());
        }
        if self.deadline.is_some_and(|end| Instant::now() >= end) {
            return Err(timed_out());
        }
        Ok(())
    }

    fn wait(&self, events: libc::c_short) -> io::Result<()> {
        loop {
            self.check()?;
            let timeout = self.deadline.map_or(-1, |end| {
                end.saturating_duration_since(Instant::now())
                    .as_millis()
                    .clamp(1, i32::MAX as u128) as i32
            });
            let mut fd = libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events,
                revents: 0,
            };
            // SAFETY: one live descriptor and writable pollfd.
            let n = unsafe { libc::poll(&mut fd, 1, timeout) };
            self.check()?;
            if n > 0 {
                return Ok(());
            }
            if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error());
            }
        }
    }
}

impl Read for Connection<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.check()?;
            match self.stream.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(libc::POLLIN)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
}

impl Write for Connection<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            self.check()?;
            match self.stream.write(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(libc::POLLOUT)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.check()
    }
}

impl Drop for Connection<'_> {
    fn drop(&mut self) {
        if let Some(stop) = self.stop {
            stop.detach();
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("could not reach the docker socket at {path}: {source}")]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("docker answered {status} for {path}")]
    Status { status: String, path: String },
    #[error("malformed response: {0}")]
    Malformed(String),
}

/// How the body that follows is framed.
///
/// Docker uses both, and which one depends on the endpoint: a complete document
/// like `/containers/json` carries a Content-Length, while `/events` streams and
/// must be chunked. Accepting only one of them makes half the API unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    Chunked,
    Length(usize),
}

/// Total budget for a finite request or an event stream's connection/headers.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn send<'a>(
    socket: &Path,
    path: &str,
    deadline: Instant,
    stop: Option<&'a Stop>,
) -> Result<(BufReader<Connection<'a>>, Body), HttpError> {
    let mut stream =
        Connection::connect(socket, deadline, stop).map_err(|source| HttpError::Connect {
            path: socket.display().to_string(),
            source,
        })?;

    // HTTP/1.1 because the event stream needs a connection that stays open and
    // a server that is allowed to chunk. `Host` is required by 1.1 and ignored
    // by Docker.
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: docker\r\nAccept: application/json\r\n\r\n"
    )?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    if !status.contains(" 200") {
        return Err(HttpError::Status {
            status: status.trim().to_string(),
            path: path.to_string(),
        });
    }

    let mut body = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            return Err(HttpError::Malformed("headers ended early".into()));
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let lower = header.to_ascii_lowercase();
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            body = Some(Body::Chunked);
        } else if let Some(len) = lower.strip_prefix("content-length:") {
            let len = len
                .trim()
                .parse()
                .map_err(|_| HttpError::Malformed(format!("bad content-length {len:?}")))?;
            body = Some(Body::Length(len));
        }
    }

    // Neither header means the body runs to end of connection, which on a
    // keep-alive connection means it never ends. Refusing is better than
    // hanging on a response we cannot delimit.
    let body = body.ok_or_else(|| {
        HttpError::Malformed("response had neither Content-Length nor chunked encoding".into())
    })?;
    Ok((reader, body))
}

/// Fetches a complete JSON document.
pub fn get_json(socket: &Path, path: &str) -> Result<serde_json::Value, HttpError> {
    get_json_until(socket, path, Instant::now() + REQUEST_TIMEOUT)
}

/// Fetches a finite response within the caller's budget, including connect,
/// headers, writes and body reads.
pub fn get_json_until(
    socket: &Path,
    path: &str,
    deadline: Instant,
) -> Result<serde_json::Value, HttpError> {
    let (mut reader, body) = send(socket, path, deadline, None)?;
    let mut bytes = Vec::new();
    match body {
        Body::Length(len) => {
            bytes.resize(len, 0);
            reader.read_exact(&mut bytes)?;
        }
        Body::Chunked => {
            while let Some(chunk) = read_chunk(&mut reader)? {
                bytes.extend_from_slice(&chunk);
            }
        }
    }
    serde_json::from_slice(&bytes).map_err(|e| HttpError::Malformed(e.to_string()))
}

/// Calls `on_event` for each newline-delimited JSON object as it arrives.
///
/// Returns when the stream ends, which is how a dropped connection surfaces:
/// the caller reconnects rather than this looping forever.
pub fn stream_json(
    socket: &Path,
    path: &str,
    stop: Option<&Stop>,
    mut on_event: impl FnMut(serde_json::Value),
) -> Result<(), HttpError> {
    stream_json_until(
        socket,
        path,
        stop,
        Instant::now() + REQUEST_TIMEOUT,
        &mut on_event,
    )
}

fn stream_json_until(
    socket: &Path,
    path: &str,
    stop: Option<&Stop>,
    deadline: Instant,
    on_event: &mut impl FnMut(serde_json::Value),
) -> Result<(), HttpError> {
    if stop.is_some_and(Stop::asked) {
        return Ok(());
    }
    let result = (|| {
        let (mut reader, body) = send(socket, path, deadline, stop)?;
        if body != Body::Chunked {
            return Err(HttpError::Malformed(
                "an event stream must be chunked".into(),
            ));
        }
        // Header setup was bounded and cancellable. Only the body is endless.
        reader.get_mut().deadline = None;
        stream_body(&mut reader, on_event)
    })();
    if stop.is_some_and(Stop::asked) {
        Ok(())
    } else {
        result
    }
}

fn stream_body(
    reader: &mut impl BufRead,
    on_event: &mut impl FnMut(serde_json::Value),
) -> Result<(), HttpError> {
    // A chunk boundary is not a message boundary — Docker may split one event
    // across two chunks or pack several into one — so events are recovered from
    // the byte stream by newline, not by chunk.
    let mut pending: Vec<u8> = Vec::new();

    while let Some(chunk) = read_chunk(reader)? {
        pending.extend_from_slice(&chunk);
        while let Some(newline) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=newline).collect();
            let line = &line[..line.len() - 1];
            if line.is_empty() {
                continue;
            }
            match serde_json::from_slice(line) {
                Ok(value) => on_event(value),
                Err(e) => tracing::debug!(%e, "unparseable docker event"),
            }
        }
    }
    Ok(())
}

/// Reads one chunk, or `None` at the terminating zero-length chunk.
fn read_chunk(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, HttpError> {
    let mut size_line = String::new();
    if reader.read_line(&mut size_line)? == 0 {
        return Ok(None);
    }
    // The size line may carry chunk extensions after a semicolon, which nothing
    // we talk to uses but which are legal and free to ignore.
    let size_text = size_line.trim().split(';').next().unwrap_or("").trim();
    if size_text.is_empty() {
        return Ok(None);
    }
    let size = usize::from_str_radix(size_text, 16)
        .map_err(|_| HttpError::Malformed(format!("bad chunk size {size_text:?}")))?;
    if size == 0 {
        return Ok(None);
    }

    let mut buf = vec![0u8; size];
    reader.read_exact(&mut buf)?;
    // Each chunk is followed by its own CRLF, which is not part of the body.
    let mut trailer = [0u8; 2];
    reader.read_exact(&mut trailer)?;
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::{Arc, mpsc};
    use std::thread;

    struct Server {
        path: PathBuf,
        task: Option<thread::JoinHandle<()>>,
    }
    impl Server {
        fn new(serve: impl FnOnce(UnixStream) + Send + 'static) -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let path = PathBuf::from(format!(
                "/tmp/lighter-http-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let listener = UnixListener::bind(&path).unwrap();
            let task = thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                // Consume the request before the test's response behavior.
                let mut reader = BufReader::new(stream);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                serve(reader.into_inner());
            });
            Self {
                path,
                task: Some(task),
            }
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.take().unwrap().join().unwrap();
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn is_timeout(error: &HttpError) -> bool {
        matches!(error, HttpError::Io(e) | HttpError::Connect { source: e, .. } if e.kind() == io::ErrorKind::TimedOut)
    }

    #[test]
    fn a_stalled_response_obeys_the_callers_deadline() {
        let (done, wait) = mpsc::channel();
        let server = Server::new(move |_stream| {
            let _ = wait.recv_timeout(Duration::from_secs(2));
        });
        let start = Instant::now();
        let error = get_json_until(&server.path, "/version", start + Duration::from_millis(100))
            .unwrap_err();
        done.send(()).unwrap();
        assert!(is_timeout(&error), "{error}");
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn trickled_headers_and_bodies_do_not_extend_the_deadline() {
        for prefix in ["", "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n"] {
            let server = Server::new(move |mut stream| {
                let _ = stream.write_all(prefix.as_bytes());
                for _ in 0..100 {
                    if stream.write_all(b" ").is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            });
            let start = Instant::now();
            let error =
                get_json_until(&server.path, "/version", start + Duration::from_millis(120))
                    .unwrap_err();
            assert!(is_timeout(&error), "{error}");
            assert!(start.elapsed() < Duration::from_secs(1));
        }
    }

    #[test]
    fn event_headers_can_be_cancelled_before_the_body_exists() {
        let (connected, ready) = mpsc::channel();
        let (done, wait) = mpsc::channel();
        let server = Server::new(move |_stream| {
            connected.send(()).unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(2));
        });
        let stop = Stop::new();
        let handle = Arc::clone(&stop);
        let path = server.path.clone();
        let (finished, result) = mpsc::channel();
        let client = thread::spawn(move || {
            let answer = stream_json(&path, "/events", Some(&handle), |_| {});
            finished.send(answer).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        stop.stop();
        result
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        done.send(()).unwrap();
        client.join().unwrap();
    }

    #[test]
    fn event_headers_have_a_deadline_even_without_cancellation() {
        let (done, wait) = mpsc::channel();
        let server = Server::new(move |_stream| {
            let _ = wait.recv_timeout(Duration::from_secs(2));
        });
        let error = stream_json_until(
            &server.path,
            "/events",
            None,
            Instant::now() + Duration::from_millis(100),
            &mut |_| {},
        )
        .unwrap_err();
        done.send(()).unwrap();
        assert!(is_timeout(&error), "{error}");
    }

    #[test]
    fn established_event_body_outlives_setup_deadline_and_is_cancellable() {
        let server = Server::new(move |mut stream| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .unwrap();
            thread::sleep(Duration::from_millis(250));
            let _ = stream.write_all(b"3\r\n{}\n\r\n");
            let mut byte = [0];
            let _ = stream.read(&mut byte); // cancellation closes the connection
        });
        let stop = Stop::new();
        let mut seen = 0;
        stream_json_until(
            &server.path,
            "/events",
            Some(&stop),
            Instant::now() + Duration::from_millis(150),
            &mut |_| {
                seen += 1;
                stop.stop();
            },
        )
        .unwrap();
        assert_eq!(seen, 1);
        assert!(stop.stream.lock().unwrap().is_none());
    }

    #[test]
    fn finite_json_accepts_length_and_chunked_bodies() {
        for response in [
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\n{\r\n1\r\n}\r\n0\r\n\r\n",
        ] {
            let server = Server::new(move |mut stream| {
                stream.write_all(response.as_bytes()).unwrap();
            });
            assert_eq!(
                get_json(&server.path, "/version").unwrap(),
                serde_json::json!({})
            );
        }
    }

    #[test]
    fn a_blocked_write_has_a_deadline_too() {
        let (stream, _unread) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut connection = Connection {
            stream,
            deadline: Some(Instant::now() + Duration::from_millis(100)),
            stop: None,
        };
        let error = connection.write_all(&vec![0; 8 << 20]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn cancelling_a_finished_stream_does_not_shutdown_a_reused_fd() {
        let stop = Stop::new();
        {
            let (stream, _peer) = UnixStream::pair().unwrap();
            stop.attach(&stream).unwrap();
            let _connection = Connection {
                stream,
                deadline: None,
                stop: Some(&stop),
            };
        }
        let (mut left, mut right) = UnixStream::pair().unwrap();
        stop.stop();
        left.write_all(b"x").unwrap();
        let mut byte = [0];
        right.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b'x']);
    }
}
