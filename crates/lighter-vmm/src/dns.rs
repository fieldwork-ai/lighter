//! DNS for the guest, answered by the Mac's resolver.
//!
//! The agent serves DNS inside the guest and carries every query here over
//! one vsock stream, framed `[len u16][id u16][query]`; each is sent to the
//! nameserver macOS is configured with and the reply goes back the same way.
//! That resolver is a VPN's when one is up, so a container resolves what the
//! Mac resolves. Raw forwarding rather than `getaddrinfo`: every record
//! type works, and the reply is the resolver's own.

use std::io::{Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use crate::virtio::vsock::{Accepted, VsockShared, pump};

/// The vsock port the agent dials for DNS.
pub const DNS_PORT: u32 = 2379;

/// The first nameserver in the Mac's resolver configuration, re-read per
/// query batch so a VPN coming or going is followed.
fn nameserver() -> SocketAddr {
    let text = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if words.next() == Some("nameserver")
            && let Some(addr) = words.next()
            && let Ok(ip) = addr.parse::<std::net::IpAddr>()
        {
            return SocketAddr::new(ip, 53);
        }
    }
    SocketAddr::new(std::net::Ipv4Addr::new(1, 1, 1, 1).into(), 53)
}

/// Starts answering the agent's DNS stream.
pub fn start(shared: Arc<VsockShared>) -> std::io::Result<()> {
    let accepted = shared.listen(DNS_PORT);
    std::thread::Builder::new()
        .name("dns-accept".into())
        .spawn(move || {
            for Accepted { key } in accepted {
                let (device_side, stream) = match UnixStream::pair() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                crate::sockbuf::widen(&device_side);
                crate::sockbuf::widen(&stream);
                let pumped = shared.clone();
                let _ = std::thread::Builder::new()
                    .name("dns-pump".into())
                    .spawn(move || pump(pumped, key, device_side));
                let _ = std::thread::Builder::new()
                    .name("dns".into())
                    .spawn(move || serve(stream));
            }
        })?;
    Ok(())
}

/// One DNS stream: queries in, forwarded on a UDP socket, replies out.
fn serve(mut stream: UnixStream) {
    let Ok(udp) = UdpSocket::bind("0.0.0.0:0") else { return };
    let _ = udp.set_read_timeout(Some(Duration::from_secs(5)));
    let Ok(mut writer) = stream.try_clone() else { return };
    let Ok(udp_reader) = udp.try_clone() else { return };
    // Replies come back on their own thread, matched to the query by the
    // id the agent gave it, which rides in the DNS transaction id field on
    // the way to the resolver and back.
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok((n, _)) = udp_reader.recv_from(&mut buf) {
            if n < 12 {
                continue;
            }
            let id = u16::from_be_bytes([buf[0], buf[1]]);
            let mut frame = Vec::with_capacity(4 + n);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
            frame.extend_from_slice(&id.to_be_bytes());
            frame.extend_from_slice(&buf[..n]);
            if writer.write_all(&frame).is_err() {
                return;
            }
        }
    });
    let mut header = [0u8; 4];
    loop {
        if stream.read_exact(&mut header).is_err() {
            return;
        }
        let len = u16::from_be_bytes([header[0], header[1]]) as usize;
        let id = u16::from_be_bytes([header[2], header[3]]);
        let mut query = vec![0u8; len];
        if stream.read_exact(&mut query).is_err() {
            return;
        }
        if query.len() < 12 {
            continue;
        }
        // Our id replaces the client's transaction id for the trip; the
        // agent restores nothing because the client's id was never sent.
        query[0..2].copy_from_slice(&id.to_be_bytes());
        let _ = udp.send_to(&query, nameserver());
    }
}
