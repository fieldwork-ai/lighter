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
//! Every request leaves the link thread at once: a pool of workers does the
//! syscalls and hands the reply back to the link.

use std::collections::HashMap;
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
                })),
            );
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
                let conn = served.lock().expect("share poisoned").conns.first().copied();
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

    fn request(&self, conn: ConnId, request: Vec<u8>) {
        let served = self.conns.lock().expect("share conns poisoned").get(&conn).cloned();
        let (Some(served), Some(link)) = (served, self.link.get().cloned()) else {
            return;
        };
        let server = served.lock().expect("share poisoned").server.clone();
        crate::workers::run("share", crate::qos::CONNECTION_STACK, move || {
            let mut reply = Vec::with_capacity(4096);
            server.dispatch(&request, &mut reply);
            if !reply.is_empty() {
                link.share_reply(conn, reply);
            }
        });
    }

    fn close(&self, conn: ConnId) {
        let served = self.conns.lock().expect("share conns poisoned").remove(&conn);
        if let Some(served) = served {
            let mut s = served.lock().expect("share poisoned");
            s.conns.retain(|c| *c != conn);
            if s.conns.is_empty() {
                s.server.set_push_invalidation(false);
            }
        }
    }
}
