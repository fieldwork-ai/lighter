//! Just enough HTTP to talk to the Docker API over a unix socket.
//!
//! Not a general client, and not trying to be. The whole vocabulary is two
//! request shapes against a socket on this machine: fetch a JSON document, and
//! read an endless stream of JSON events. A real client crate would bring an
//! async runtime and a TLS stack to a conversation that needs neither.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

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

/// Sends a request and returns the connection positioned at the body.
fn send(socket: &Path, path: &str) -> Result<(BufReader<UnixStream>, Body), HttpError> {
    let mut stream = UnixStream::connect(socket).map_err(|source| HttpError::Connect {
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
    let (mut reader, body) = send(socket, path)?;
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
    mut on_event: impl FnMut(serde_json::Value),
) -> Result<(), HttpError> {
    let (mut reader, body) = send(socket, path)?;
    if body != Body::Chunked {
        return Err(HttpError::Malformed(
            "an event stream must be chunked".into(),
        ));
    }
    // A chunk boundary is not a message boundary — Docker may split one event
    // across two chunks or pack several into one — so events are recovered from
    // the byte stream by newline, not by chunk.
    let mut pending: Vec<u8> = Vec::new();

    while let Some(chunk) = read_chunk(&mut reader)? {
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
fn read_chunk(reader: &mut BufReader<UnixStream>) -> Result<Option<Vec<u8>>, HttpError> {
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
