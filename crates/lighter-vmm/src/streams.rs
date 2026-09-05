//! Published ports: a port a container publishes appears on the Mac.
//!
//! The link carries a container's own connections out (`link.rs`); this is
//! the other direction. Each port Docker reports is bound on the Mac by us,
//! and every connection it accepts becomes a connection into the guest, to
//! the agent's inbound port, which is told the published port and connects
//! on to the container. No proxy inside the VM double-copies the bytes.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use crate::link::Link;

pub struct PortMapper {
    link: Arc<Link>,
    listeners: std::sync::Mutex<std::collections::HashMap<u16, Arc<std::sync::atomic::AtomicBool>>>,
}

impl PortMapper {
    pub fn new(link: Arc<Link>) -> Arc<PortMapper> {
        Arc::new(PortMapper {
            link,
            listeners: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }
}

impl lighter_docker::PortMapper for PortMapper {
    fn expose(&self, port: u16) -> Result<(), String> {
        let mut listeners = self.listeners.lock().expect("port mapper poisoned");
        if listeners.contains_key(&port) {
            return Ok(());
        }
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))?;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        listeners.insert(port, stop.clone());
        let link = self.link.clone();
        std::thread::Builder::new()
            .name(format!("port-{port}"))
            .spawn(move || {
                for accepted in listener.incoming() {
                    if stop.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let Ok(mac) = accepted else { continue };
                    link.carry_inbound(port, mac);
                }
            })
            .map_err(|e| e.to_string())?;
        tracing::info!(port, "port published through a stream");
        Ok(())
    }

    fn unexpose(&self, port: u16) -> Result<(), String> {
        let stop = self
            .listeners
            .lock()
            .expect("port mapper poisoned")
            .remove(&port);
        if let Some(stop) = stop {
            stop.store(true, std::sync::atomic::Ordering::Release);
            // Wake the accept loop so it sees the flag.
            let _ = TcpStream::connect_timeout(
                &SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
                Duration::from_millis(200),
            );
        }
        Ok(())
    }
}
