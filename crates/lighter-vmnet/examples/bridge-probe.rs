//! Asks a helper for a bridge and counts the frames that arrive.
//!
//!     cargo run -p lighter-vmnet --example bridge-probe -- <socket> <interface> [seconds]
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let socket = args.get(1).expect("socket");
    let interface = args.get(2).expect("interface");
    let seconds: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);
    let mac = [0x02, 0x6c, 0x69, 0x67, 0x68, 0x75];
    let held = match lighter_vmnet::helper::connect(std::path::Path::new(socket), interface, mac) {
        Ok(h) => h,
        Err(e) => {
            println!("refused: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "bridged {interface}: mtu {} max {}",
        held.mtu, held.max_packet
    );
    let fd = held.frames.as_raw_fd();
    let tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 200_000,
    };
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            std::ptr::addr_of!(tv).cast(),
            size_of::<libc::timeval>() as u32,
        )
    };
    let (mut frames, mut multicast, mut mdns) = (0, 0, 0);
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut buf = [0u8; 2048];
    while Instant::now() < end {
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 14 {
            continue;
        }
        frames += 1;
        if buf[0] & 1 == 1 {
            multicast += 1;
        }
        if n > 42 && buf[12] == 8 && buf[13] == 0 && buf[23] == 17 {
            let ihl = ((buf[14] & 15) * 4) as usize;
            let dport = u16::from_be_bytes([buf[14 + ihl + 2], buf[14 + ihl + 3]]);
            if dport == 5353 {
                mdns += 1;
            }
        }
    }
    println!("{frames} frames in {seconds}s ({multicast} multicast/broadcast, {mdns} mDNS)");
}
