//! The Neural Engine for containers: ONNX Runtime on the host, reached from
//! the guest over a TCP stream.
//!
//! A container's ONNX Runtime loads lighter's plugin execution provider,
//! which claims the graph, serialises it and connects here — to the Mac at
//! `host.docker.internal` on the port the kernel command line names
//! (`lighter.ane=<port>`), which rides the same streams every container
//! connection to the Mac does. The host loads the model with the CoreML
//! provider, which puts what it can on the Neural Engine, and runs it;
//! tensors cross as bytes (`protocol`).
//!
//! Loopback only, an ephemeral port per machine, and nothing on it but the
//! protocol: a stray local process could send a model to run, and that is the
//! whole of the exposure.

pub mod ort;
pub mod ort_sys;
pub mod protocol;

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};

use protocol::{Reader, Writer};

pub struct Server {
    port: u16,
}

impl Server {
    /// Binds loopback on a port of the system's choosing and serves from a
    /// thread. The runtime is created on the first connection, not here:
    /// ONNX Runtime and CoreML cost memory a machine that never runs a model
    /// should not pay.
    pub fn start() -> io::Result<Server> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        std::thread::Builder::new()
            .name("ane-accept".into())
            .spawn(move || {
                let runtime: Arc<OnceLock<Result<ort::Runtime, String>>> = Arc::new(OnceLock::new());
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let runtime = runtime.clone();
                    let _ = std::thread::Builder::new()
                        .name("ane-conn".into())
                        .spawn(move || serve(stream, &runtime));
                }
            })?;
        tracing::info!(port, "neural engine service listening");
        Ok(Server { port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

fn read_frame(stream: &mut TcpStream) -> io::Result<Option<(u32, Vec<u8>)>> {
    let mut header = [0u8; protocol::HEADER_LEN];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let kind = u32::from_le_bytes(header[..4].try_into().unwrap());
    let len = u64::from_le_bytes(header[4..].try_into().unwrap());
    if len > protocol::MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes refused")));
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload)?;
    Ok(Some((kind, payload)))
}

fn serve(mut stream: TcpStream, runtime: &OnceLock<Result<ort::Runtime, String>>) {
    let _ = stream.set_nodelay(true);
    let mut sessions: HashMap<u64, ort::Session> = HashMap::new();
    let mut next_id = 1u64;
    loop {
        let (kind, payload) = match read_frame(&mut stream) {
            Ok(Some(f)) => f,
            Ok(None) => return,
            Err(e) => {
                tracing::debug!(%e, "neural engine stream ended");
                return;
            }
        };
        let reply = match handle(kind, &payload, runtime, &mut sessions, &mut next_id) {
            Ok(body) => protocol::frame(kind, &body),
            Err(msg) => {
                tracing::debug!(kind, %msg, "neural engine request failed");
                protocol::frame(protocol::ERR, msg.as_bytes())
            }
        };
        if stream.write_all(&reply).is_err() {
            return;
        }
    }
}

fn handle(
    kind: u32,
    payload: &[u8],
    runtime: &OnceLock<Result<ort::Runtime, String>>,
    sessions: &mut HashMap<u64, ort::Session>,
    next_id: &mut u64,
) -> Result<Vec<u8>, String> {
    match kind {
        protocol::LOAD => {
            let rt = runtime.get_or_init(ort::Runtime::new).as_ref().map_err(|e| e.clone())?;
            let session = rt.load(payload)?;
            let id = *next_id;
            *next_id += 1;
            tracing::info!(session = id, bytes = payload.len(), "neural engine model loaded");
            sessions.insert(id, session);
            let mut w = Writer::new();
            w.u64(id);
            Ok(w.0)
        }
        protocol::RUN => {
            let mut r = Reader::new(payload);
            let id = r.u64()?;
            let n = r.u32()? as usize;
            let mut inputs = Vec::with_capacity(n);
            for _ in 0..n {
                inputs.push(r.tensor()?);
            }
            let session = sessions.get(&id).ok_or_else(|| format!("no session {id}"))?;
            let outputs = session.run(&inputs)?;
            let mut w = Writer::new();
            w.u32(outputs.len() as u32);
            for t in &outputs {
                w.tensor(t);
            }
            Ok(w.0)
        }
        protocol::CLOSE => {
            let id = Reader::new(payload).u64()?;
            sessions.remove(&id);
            Ok(Vec::new())
        }
        other => Err(format!("unknown request kind {other:#x}")),
    }
}

/// Kept so a machine can hold its server; the thread runs for the process.
pub type Shared = Arc<Mutex<Option<Server>>>;
