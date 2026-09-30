//! Serves a real USB device through the USB/IP server to a stand-in guest on
//! a socket, without a VM: seize, read its descriptors, configure it, and
//! give it back, checking each reply. For a device on the desk, not CI.
//!
//!   cargo run -p lighter-vmm --example usb-probe -- 303a:831a
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use lighter_vmm::usb::iousb::{self, IoUsbDevice};
use lighter_vmm::usb::server::Server;
use lighter_vmm::usb::usbip::{Command, DIR_IN, DIR_OUT, Reply, Submit, encode, parse_reply};

struct Guest {
    s: UnixStream,
    buf: Vec<u8>,
    ins: std::collections::HashSet<u32>,
    seq: u32,
}

impl Guest {
    fn submit(&mut self, direction: u32, ep: u8, setup: [u8; 8], length: usize, data: Vec<u8>) -> Reply {
        self.seq += 1;
        if direction == DIR_IN {
            self.ins.insert(self.seq);
        }
        let c = Command::Submit(Submit {
            seqnum: self.seq,
            devid: 1,
            direction,
            ep,
            transfer_flags: 0,
            length,
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            setup,
            data,
            iso: Vec::new(),
        });
        let t0 = Instant::now();
        self.s.write_all(&encode(&c)).unwrap();
        let r = self.reply();
        println!("  {:?} in {:?}", r, t0.elapsed());
        r
    }

    fn reply(&mut self) -> Reply {
        loop {
            let ins = self.ins.clone();
            if let Some((r, used)) = parse_reply(&self.buf, |s| ins.contains(&s)) {
                self.buf.drain(..used);
                return r;
            }
            let mut chunk = [0u8; 4096];
            let n = self.s.read(&mut chunk).expect("reply");
            assert!(n > 0, "the session ended");
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

fn main() {
    let spec = std::env::args().nth(1).expect("vid:pid");
    let (vid, pid) = spec.split_once(':').expect("vid:pid");
    let (vid, pid) = (u16::from_str_radix(vid, 16).unwrap(), u16::from_str_radix(pid, 16).unwrap());
    let info = iousb::list()
        .into_iter()
        .find(|d| d.vendor_id == vid && d.product_id == pid)
        .expect("no such device");
    println!("{info:?}");
    if let Some(port) = &info.callout {
        println!("port {port} held by {:?}", iousb::port_holder(port));
    }
    let server = Server::start().unwrap();
    let sink = server.sink();
    let device = IoUsbDevice::open(info.registry_id, sink.clone()).expect("seize");
    let (guest, host) = UnixStream::pair().unwrap();
    guest.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    server.serve(&sink, host, Box::new(device));
    let port_gone = info.callout.as_deref().is_some_and(|p| !std::path::Path::new(p).exists());
    println!("seized; macOS's port removed: {port_gone}");
    let mut g = Guest { s: guest, buf: Vec::new(), ins: Default::default(), seq: 0 };
    println!("device descriptor:");
    g.submit(DIR_IN, 0, [0x80, 6, 0, 1, 0, 0, 18, 0], 18, vec![]);
    println!("configuration descriptor:");
    g.submit(DIR_IN, 0, [0x80, 6, 0, 2, 0, 0, 255, 0], 255, vec![]);
    println!("set configuration 1:");
    g.submit(DIR_OUT, 0, [0x00, 9, 1, 0, 0, 0, 0, 0], 0, vec![]);
    println!("product string:");
    g.submit(DIR_IN, 0, [0x80, 6, 2, 3, 0x09, 0x04, 255, 0], 255, vec![]);
    drop(g);
    // The session ends with the guest; the device goes back to macOS.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        if info.callout.as_deref().is_none_or(|p| std::path::Path::new(p).exists()) && !server.serving(&sink) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!(
        "released after {:?}; macOS's port back: {}",
        t0.elapsed(),
        info.callout.as_deref().is_none_or(|p| std::path::Path::new(p).exists())
    );
}
