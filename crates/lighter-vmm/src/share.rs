//! Host directories in the guest: the server behind the link.
//!
//! `lighter_fs::Server` turns a FUSE request into a reply and knows nothing
//! about how requests arrive. Here they arrive over the link: the guest
//! kernel's `lighterfs` transport opens a few TCP connections to the share
//! port, names the tag it wants on each, and writes requests framed by the
//! length in their own header; replies go back the same way, and
//! invalidations from FSEvents ride the first live connection as ordinary
//! FUSE notifications (`unique` zero), which the kernel already parses.
//!
//! A request is answered on the link thread itself when nothing else is in
//! flight for its share and it is small — a lookup, a stat, a directory
//! read, a short read or write — because the two thread hops that a worker
//! costs (to it, and back to the link) are more than the syscall; anything
//! else goes to a pool of workers, which hand the reply back to the link.
//! Off until measured: `LIGHTER_FS_INLINE=1` turns it on.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::link::{ConnId, Link, ShareTransport};

#[derive(Debug, Clone)]
pub struct Share {
    pub tag: String,
    pub path: std::path::PathBuf,
}

struct Served {
    server: Arc<lighter_fs::Server>,
    /// The connections carrying this share, oldest first; notifications go
    /// on the first.
    conns: Vec<ConnId>,
    /// Requests handed to workers and not yet answered.
    in_flight: Arc<AtomicUsize>,
}

/// Requests up to this size may be answered inline; larger ones carry file
/// data worth a worker.
const INLINE_MAX: usize = 64 << 10;
/// FUSE_FSYNC and FUSE_FSYNCDIR wait on the disk; never inline.
const OP_FSYNC: u32 = 20;
const OP_FSYNCDIR: u32 = 30;

fn inline_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("LIGHTER_FS_INLINE")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

pub struct Shares {
    by_tag: HashMap<String, Arc<Mutex<Served>>>,
    conns: Mutex<HashMap<ConnId, Arc<Mutex<Served>>>>,
    link: OnceLock<Arc<Link>>,
}

impl Shares {
    pub fn new(shares: &[Share]) -> std::io::Result<Shares> {
        let mut by_tag = HashMap::new();
        for share in shares {
            let server = Arc::new(lighter_fs::Server::new(&share.path)?);
            by_tag.insert(
                share.tag.clone(),
                Arc::new(Mutex::new(Served {
                    server,
                    conns: Vec::new(),
                    in_flight: Arc::new(AtomicUsize::new(0)),
                })),
            );
        }
        // With `LIGHTER_FS_STATS` set, every server's histogram every ten
        // seconds (`LIGHTER_FS_STATS_EVERY` overrides), as the old device did.
        if std::env::var_os("LIGHTER_FS_STATS").is_some() {
            let every = std::env::var("LIGHTER_FS_STATS_EVERY")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(10);
            let servers: Vec<Arc<lighter_fs::Server>> = by_tag
                .values()
                .map(|s| s.lock().expect("share poisoned").server.clone())
                .collect();
            std::thread::Builder::new()
                .name("fs-stats".into())
                .spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_secs(every));
                    for server in &servers {
                        server.log_stats();
                    }
                })?;
        }
        Ok(Shares {
            by_tag,
            conns: Mutex::new(HashMap::new()),
            link: OnceLock::new(),
        })
    }

    /// The link the replies and notifications go through. Once it is known,
    /// every server's invalidations have somewhere to go.
    pub fn attach(&self, link: Arc<Link>) {
        if self.link.set(link).is_err() {
            return;
        }
        for served in self.by_tag.values() {
            let server = served.lock().expect("share poisoned").server.clone();
            let sink = server.notifications().clone();
            let served = served.clone();
            let link = self.link.get().expect("set above").clone();
            let drain = server.notifications().clone();
            sink.set_waker(Arc::new(move || {
                let conn = served
                    .lock()
                    .expect("share poisoned")
                    .conns
                    .first()
                    .copied();
                let Some(conn) = conn else {
                    // Nobody is mounted: nothing cached, nothing to invalidate.
                    while drain.take().is_some() {}
                    return;
                };
                while let Some(message) = drain.take() {
                    link.share_reply(conn, message);
                }
            }));
        }
    }
}

impl ShareTransport for Shares {
    fn open(&self, conn: ConnId, tag: &str) -> bool {
        let Some(served) = self.by_tag.get(tag) else {
            return false;
        };
        {
            let mut s = served.lock().expect("share poisoned");
            s.conns.push(conn);
            if s.conns.len() == 1 {
                // The first connection is the channel invalidations ride;
                // with it the guest may cache for as long as it likes.
                s.server.set_push_invalidation(true);
            }
        }
        self.conns
            .lock()
            .expect("share conns poisoned")
            .insert(conn, served.clone());
        true
    }

    fn request(&self, conn: ConnId, request: Vec<u8>) -> Option<Vec<u8>> {
        let served = self
            .conns
            .lock()
            .expect("share conns poisoned")
            .get(&conn)
            .cloned();
        let (Some(served), Some(link)) = (served, self.link.get().cloned()) else {
            return None;
        };
        let (server, in_flight) = {
            let s = served.lock().expect("share poisoned");
            (s.server.clone(), s.in_flight.clone())
        };
        let opcode = request
            .get(4..8)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .unwrap_or(0);
        let alone = in_flight.load(Ordering::Acquire) == 0;
        if inline_enabled()
            && alone
            && request.len() <= INLINE_MAX
            && opcode != OP_FSYNC
            && opcode != OP_FSYNCDIR
        {
            let mut reply = Vec::with_capacity(4096);
            server.dispatch(&request, &mut reply);
            return if reply.is_empty() { None } else { Some(reply) };
        }
        in_flight.fetch_add(1, Ordering::AcqRel);
        crate::workers::run("share", crate::qos::CONNECTION_STACK, move || {
            let mut reply = Vec::with_capacity(4096);
            server.dispatch(&request, &mut reply);
            in_flight.fetch_sub(1, Ordering::AcqRel);
            if !reply.is_empty() {
                link.share_reply(conn, reply);
            }
        });
        None
    }

    fn close(&self, conn: ConnId) {
        let served = self
            .conns
            .lock()
            .expect("share conns poisoned")
            .remove(&conn);
        if let Some(served) = served {
            let mut s = served.lock().expect("share poisoned");
            s.conns.retain(|c| *c != conn);
            if s.conns.is_empty() {
                s.server.set_push_invalidation(false);
            }
        }
    }
}
