//! The root helper's protocol, both sides of it.
//!
//! The client connects to the helper's socket and sends one line,
//! `lighter-bridge <version> <interface> <mac>`. The helper answers
//! `ok <mtu> <max_packet>` with the frames socket attached (`SCM_RIGHTS`),
//! or `error <why>`. The connection then stays open and carries nothing:
//! it is the interface's lifetime, so a client that exits, or crashes,
//! stops its bridge.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Bumped when either side's half of the conversation changes.
pub const VERSION: u32 = 1;
/// Where the installed helper listens (launchd owns the socket).
pub const SOCKET: &str = "/var/run/dev.lighter.bridge.sock";

/// A bridge the helper holds for us: the frames socket, and the
/// connection whose closing stops it.
pub struct Held {
    pub frames: OwnedFd,
    pub mtu: u32,
    pub max_packet: u32,
    _control: UnixStream,
}

/// The first line a client sends.
pub fn hello(interface: &str, mac: [u8; 6]) -> String {
    format!("lighter-bridge {VERSION} {interface} {}\n", mac_text(mac))
}

/// What a hello asks for: (version, interface, mac).
pub fn parse_hello(line: &str) -> Result<(u32, String, [u8; 6]), String> {
    let mut words = line.split_whitespace();
    if words.next() != Some("lighter-bridge") {
        return Err("not a lighter-bridge hello".into());
    }
    let version = words
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or("no version")?;
    let interface = words.next().ok_or("no interface")?.to_string();
    if interface.is_empty()
        || interface.len() > 15
        || !interface.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err(format!("bad interface {interface:?}"));
    }
    let mac = words.next().and_then(parse_mac).ok_or("bad mac")?;
    Ok((version, interface, mac))
}

pub fn mac_text(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let parts: Vec<u8> = text
        .split(':')
        .filter_map(|p| u8::from_str_radix(p, 16).ok())
        .collect();
    <[u8; 6]>::try_from(parts.as_slice())
        .ok()
        .filter(|_| text.split(':').count() == 6)
}

/// Asks the helper at `socket` for a bridge on `interface`.
pub fn connect(socket: &Path, interface: &str, mac: [u8; 6]) -> io::Result<Held> {
    let mut control = UnixStream::connect(socket)?;
    control.set_read_timeout(Some(Duration::from_secs(15)))?;
    control.write_all(hello(interface, mac).as_bytes())?;
    let (line, fd) = recv_line_with_fd(&control)?;
    control.set_read_timeout(None)?;
    let line = line.trim();
    if let Some(why) = line.strip_prefix("error ") {
        return Err(io::Error::other(why.to_string()));
    }
    let mut words = line.split_whitespace();
    if words.next() != Some("ok") {
        return Err(io::Error::other(format!("unexpected answer {line:?}")));
    }
    let mtu = words.next().and_then(|v| v.parse().ok()).unwrap_or(1500);
    let max_packet = words.next().and_then(|v| v.parse().ok()).unwrap_or(1514);
    let frames = fd.ok_or_else(|| io::Error::other("the helper sent no frames socket"))?;
    Ok(Held {
        frames,
        mtu,
        max_packet,
        _control: control,
    })
}

/// Sends one line, with `fd` attached when given.
pub fn send_line_with_fd(stream: &UnixStream, line: &str, fd: Option<RawFd>) -> io::Result<()> {
    let bytes = line.as_bytes();
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut _,
        iov_len: bytes.len(),
    };
    // Room for one descriptor's control message.
    let mut cmsg = [0u8; 64];
    // SAFETY: a zeroed msghdr filled with pointers to locals that outlive
    // the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(fd) = fd {
        // SAFETY: CMSG_SPACE and the header writes stay within `cmsg`.
        unsafe {
            let space = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as usize;
            msg.msg_control = cmsg.as_mut_ptr().cast();
            msg.msg_controllen = space as _;
            let header = libc::CMSG_FIRSTHDR(&msg);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
        }
    }
    // SAFETY: a msghdr describing live buffers.
    if unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reads one line, and a descriptor if one came with it.
pub fn recv_line_with_fd(stream: &UnixStream) -> io::Result<(String, Option<OwnedFd>)> {
    let mut buf = [0u8; 512];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cmsg = [0u8; 64];
    // SAFETY: as in send_line_with_fd.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = cmsg.len() as _;
    // SAFETY: a msghdr describing live buffers.
    let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the helper closed the connection",
        ));
    }
    let mut fd = None;
    // SAFETY: walking the control messages the kernel wrote into `cmsg`.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&msg);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let raw = std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>());
                fd = Some(OwnedFd::from_raw_fd(raw));
            }
            header = libc::CMSG_NXTHDR(&msg, header);
        }
    }
    Ok((String::from_utf8_lossy(&buf[..n as usize]).into_owned(), fd))
}

/// Reads the client's hello, a line at most 256 bytes long.
pub fn read_hello(stream: &UnixStream) -> io::Result<String> {
    let mut line = String::new();
    BufReader::new(stream.take(256)).read_line(&mut line)?;
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hello_round_trips() {
        let mac = [0x02, 0x6c, 0x69, 0x67, 0x68, 0x74];
        let (version, interface, parsed) = parse_hello(&hello("en1", mac)).unwrap();
        assert_eq!((version, interface.as_str(), parsed), (VERSION, "en1", mac));
        assert!(parse_hello("lighter-bridge 1 ../../etc 02:00:00:00:00:01").is_err());
        assert!(parse_hello("lighter-bridge 1 en1 02:00:00").is_err());
        assert!(parse_hello("GET / HTTP/1.1").is_err());
    }

    #[test]
    fn a_descriptor_travels_with_its_line() {
        let (a, b) = UnixStream::pair().unwrap();
        let (frames_near, frames_far) = crate::socket_pair().unwrap();
        send_line_with_fd(&a, "ok 1500 1514\n", Some(frames_far.as_raw_fd())).unwrap();
        let (line, fd) = recv_line_with_fd(&b).unwrap();
        assert_eq!(line, "ok 1500 1514\n");
        let fd = fd.expect("a descriptor");
        // The descriptor received is the far end: a datagram through it
        // arrives at the near end.
        // SAFETY: a small buffer on live sockets.
        unsafe { libc::send(fd.as_raw_fd(), b"x".as_ptr().cast(), 1, 0) };
        let mut got = [0u8; 4];
        assert_eq!(
            unsafe { libc::recv(frames_near.as_raw_fd(), got.as_mut_ptr().cast(), 4, 0) },
            1
        );
    }
}
