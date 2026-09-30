//! USB devices from the Mac, attached to `vhci-hcd` (0.11.0).
//!
//! The Mac opens one vsock stream per device and sends a twelve-byte header:
//! the device id `vhci` will see in the device's commands, its speed, and its
//! vendor and product ids for the log. The stream is then USB/IP's, and this
//! process hands it to the kernel: a write to `vhci-hcd`'s `attach` names a
//! free port and the socket, and from then on the kernel owns the socket and
//! the device is as good as plugged in. When the Mac ends the stream, `vhci`
//! unplugs the device.
//!
//! The same loop keeps `/dev/serial/by-id`, named exactly as udev names it,
//! from the kernel's uevents: Home Assistant and Zigbee2MQTT are configured
//! with those names, and this guest has no udev.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

pub const HEADER_LEN: usize = 12;
const VHCI: &str = "/sys/devices/platform/vhci_hcd.0";
const BY_ID: &str = "/dev/serial/by-id";

/// The header a device's stream opens with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub devid: u32,
    /// `enum usb_device_speed`: 1 low, 2 full, 3 high, 5 super, 6 super+.
    pub speed: u32,
    pub vendor: u16,
    pub product: u16,
}

impl Header {
    pub fn parse(b: &[u8; HEADER_LEN]) -> Header {
        Header {
            devid: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            speed: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            vendor: u16::from_be_bytes([b[8], b[9]]),
            product: u16::from_be_bytes([b[10], b[11]]),
        }
    }
}

/// A free `vhci` port for a device of `speed`, from the controller's status
/// table: a port whose state is `VDEV_ST_NULL` (4), on the high-speed hub for
/// a USB 2 device and the super-speed one for USB 3.
pub fn free_port(status: &str, speed: u32) -> Option<u32> {
    let hub = if speed >= 5 { "ss" } else { "hs" };
    status.lines().skip(1).find_map(|line| {
        let mut f = line.split_whitespace();
        let (h, port, sta) = (f.next()?, f.next()?, f.next()?);
        (h == hub && sta.trim_start_matches('0') == "4").then(|| port.parse().ok()).flatten()
    })
}

/// udev's `util_replace_whitespace` and `util_replace_chars` for the parts of
/// a `by-id` name: runs of whitespace become one `_` with none at either end,
/// and anything outside udev's allowed set becomes `_`.
pub fn udev_part(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join("_");
    collapsed
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "#+-.:=@_".contains(c) { c } else { '_' })
        .collect()
}

/// The `by-id` name udev gives a USB serial port: `usb-<vendor>_<model>[_<serial>]
/// -if<NN>[-port<N>]`, the vendor and model falling back to the hex ids when
/// the device has no strings, `-port` only for a USB-to-serial bridge's port.
pub fn by_id_name(
    manufacturer: Option<&str>,
    product: Option<&str>,
    serial: Option<&str>,
    id_vendor: &str,
    id_product: &str,
    interface: &str,
    port: Option<&str>,
) -> String {
    let vendor = manufacturer.map(udev_part).filter(|v| !v.is_empty()).unwrap_or_else(|| id_vendor.to_owned());
    let model = product.map(udev_part).filter(|m| !m.is_empty()).unwrap_or_else(|| id_product.to_owned());
    let mut name = format!("usb-{vendor}_{model}");
    if let Some(serial) = serial.map(udev_part).filter(|s| !s.is_empty()) {
        name.push('_');
        name.push_str(&serial);
    }
    name.push_str("-if");
    name.push_str(interface);
    if let Some(port) = port {
        name.push_str("-port");
        name.push_str(port);
    }
    name
}

fn read_attr(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name)).ok().map(|s| s.trim().to_owned())
}

/// The `by-id` name for a tty, from sysfs: the USB interface and device it
/// hangs off, or `None` for a tty that is not a USB one.
fn by_id_for(tty: &str) -> Option<String> {
    let dev = std::fs::canonicalize(format!("/sys/class/tty/{tty}/device")).ok()?;
    // ttyACM's `device` is the interface; ttyUSB's is the usb-serial port,
    // whose parent is the interface and which has a port number.
    let (interface_dir, port) = if dev.join("bInterfaceNumber").exists() {
        (dev.clone(), None)
    } else {
        (dev.parent()?.to_path_buf(), read_attr(&dev, "port_number"))
    };
    let usb = interface_dir.parent()?;
    let interface = read_attr(&interface_dir, "bInterfaceNumber")?;
    Some(by_id_name(
        read_attr(usb, "manufacturer").as_deref(),
        read_attr(usb, "product").as_deref(),
        read_attr(usb, "serial").as_deref(),
        &read_attr(usb, "idVendor")?,
        &read_attr(usb, "idProduct")?,
        &interface,
        port.as_deref(),
    ))
}

/// Links, by tty, the names this process made, so a removal takes its own.
struct Names(HashMap<String, PathBuf>);

impl Names {
    fn add(&mut self, tty: &str) {
        let Some(name) = by_id_for(tty) else { return };
        let _ = std::fs::create_dir_all(BY_ID);
        let link = Path::new(BY_ID).join(&name);
        let _ = std::fs::remove_file(&link);
        match std::os::unix::fs::symlink(format!("../../{tty}"), &link) {
            Ok(()) => {
                println!("AGENT usb tty={tty} by-id={name}");
                self.0.insert(tty.to_owned(), link);
            }
            Err(e) => eprintln!("lighter-agent: usb: cannot link {}: {e}", link.display()),
        }
    }

    fn remove(&mut self, tty: &str) {
        if let Some(link) = self.0.remove(tty) {
            let _ = std::fs::remove_file(link);
        }
    }
}

fn is_usb_tty(name: &str) -> bool {
    name.starts_with("ttyACM") || name.starts_with("ttyUSB")
}

/// The kernel's uevents, for tty adds and removes.
fn uevents() -> io::Result<OwnedFd> {
    // SAFETY: a plain socket(2) call.
    let raw = unsafe {
        libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, libc::NETLINK_KOBJECT_UEVENT)
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor we own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: zeroed sockaddr_nl is valid; group 1 is the kernel's broadcast.
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_groups = 1;
    // SAFETY: a sockaddr_nl of its own size.
    let rc = unsafe {
        libc::bind(fd.as_raw_fd(), std::ptr::addr_of!(addr).cast(), size_of::<libc::sockaddr_nl>() as libc::socklen_t)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// A tty uevent's action and device name: `add@…/ttyACM0` with its keys.
fn tty_event(msg: &[u8]) -> Option<(String, String)> {
    let mut action = None;
    let mut name = None;
    let mut tty = false;
    for field in msg.split(|&b| b == 0) {
        let field = std::str::from_utf8(field).ok()?;
        if let Some(a) = field.strip_prefix("ACTION=") {
            action = Some(a.to_owned());
        } else if let Some(n) = field.strip_prefix("DEVNAME=") {
            name = Some(n.trim_start_matches("/dev/").to_owned());
        } else if field == "SUBSYSTEM=tty" {
            tty = true;
        }
    }
    let name = name?;
    (tty && is_usb_tty(&name)).then_some((action?, name))
}

/// Attaches a device's stream to a free `vhci` port.
fn attach(fd: RawFd, header: Header) -> io::Result<u32> {
    let status = std::fs::read_to_string(format!("{VHCI}/status"))?;
    let port = free_port(&status, header.speed).ok_or_else(|| io::Error::other("no free vhci port"))?;
    std::fs::write(format!("{VHCI}/attach"), format!("{port} {fd} {} {}", header.devid, header.speed))?;
    Ok(port)
}

pub fn serve(port: u32) -> std::process::ExitCode {
    let listener = match crate::vsock::VsockListener::bind(port) {
        Ok(l) => l.into_fd(),
        Err(e) => {
            eprintln!("lighter-agent: cannot bind vsock port {port}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let events = match uevents() {
        Ok(fd) => Some(fd),
        Err(e) => {
            eprintln!("lighter-agent: usb: no uevents, so no /dev/serial/by-id: {e}");
            None
        }
    };
    // SAFETY: fcntl on a live descriptor.
    unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    let mut names = Names(HashMap::new());
    // Ports that came before this process (a restart) get their names now.
    if let Ok(entries) = std::fs::read_dir("/sys/class/tty") {
        for entry in entries.flatten() {
            let tty = entry.file_name().to_string_lossy().into_owned();
            if is_usb_tty(&tty) {
                names.add(&tty);
            }
        }
    }
    println!("AGENT usb port={port}");
    // Streams whose header is still arriving: a header is twelve bytes and
    // follows the connection at once, but it is read here, never waited for.
    let mut opening: Vec<(OwnedFd, Vec<u8>)> = Vec::new();
    loop {
        let mut fds = vec![libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        if let Some(e) = &events {
            fds.push(libc::pollfd { fd: e.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        }
        for (fd, _) in &opening {
            fds.push(libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        }
        // SAFETY: live descriptors in an array of the length given.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n < 0 {
            continue;
        }
        // Streams accepted below were not in this poll; they wait for the next.
        let polled = opening.len();
        if fds[0].revents != 0 {
            loop {
                // SAFETY: accepting on a live listener, no peer address wanted.
                let raw = unsafe { libc::accept4(listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) };
                if raw < 0 {
                    break;
                }
                // SAFETY: a fresh accepted descriptor.
                opening.push((unsafe { OwnedFd::from_raw_fd(raw) }, Vec::with_capacity(HEADER_LEN)));
            }
        }
        if let Some(e) = &events
            && fds[1].revents != 0
        {
            let mut buf = [0u8; 8192];
            loop {
                // SAFETY: reading a datagram into a buffer we own.
                let got = unsafe { libc::recv(e.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
                if got <= 0 {
                    break;
                }
                if let Some((action, tty)) = tty_event(&buf[..got as usize]) {
                    match action.as_str() {
                        "add" => names.add(&tty),
                        "remove" => names.remove(&tty),
                        _ => {}
                    }
                }
            }
        }
        let first = if events.is_some() { 2 } else { 1 };
        let mut done = Vec::new();
        for (i, (fd, head)) in opening.iter_mut().enumerate().take(polled) {
            if fds[first + i].revents == 0 {
                continue;
            }
            let mut b = [0u8; HEADER_LEN];
            let want = HEADER_LEN - head.len();
            // SAFETY: reading only what the header still needs: whatever
            // follows is the kernel's, once it has the socket.
            let got = unsafe { libc::read(fd.as_raw_fd(), b.as_mut_ptr().cast(), want) };
            if got <= 0 {
                done.push(i);
                continue;
            }
            head.extend_from_slice(&b[..got as usize]);
            if head.len() == HEADER_LEN {
                let header = Header::parse(head.as_slice().try_into().expect("twelve bytes"));
                // vhci's receive thread blocks on the socket: it wants it
                // blocking, as usbip's own attach hands it over.
                // SAFETY: fcntl on a live descriptor.
                unsafe {
                    let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
                    libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
                }
                match attach(fd.as_raw_fd(), header) {
                    Ok(p) => println!(
                        "AGENT usb attached {:04x}:{:04x} port={p} speed={}",
                        header.vendor, header.product, header.speed
                    ),
                    Err(e) => eprintln!(
                        "lighter-agent: usb: cannot attach {:04x}:{:04x}: {e}",
                        header.vendor, header.product
                    ),
                }
                // The kernel holds its own reference once attached; ours
                // goes either way.
                done.push(i);
            }
        }
        for i in done.into_iter().rev() {
            opening.swap_remove(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "hub port sta spd dev      sockfd local_busid
hs  0000 006 002 00010002 000005 0-0
hs  0001 004 000 00000000 000000 0-0
ss  0008 004 000 00000000 000000 0-0
";

    #[test]
    fn a_free_port_is_taken_on_the_hub_for_the_devices_speed() {
        assert_eq!(free_port(STATUS, 2), Some(1));
        assert_eq!(free_port(STATUS, 3), Some(1));
        assert_eq!(free_port(STATUS, 5), Some(8));
        assert_eq!(free_port("hub port sta spd dev sockfd local_busid\nhs 0000 006 002 1 5 0-0\n", 2), None);
    }

    /// The names Linux hosts give these sticks, so compose files move over.
    #[test]
    fn by_id_names_are_udevs() {
        assert_eq!(
            by_id_name(Some("Nabu Casa"), Some("ZBT-2"), Some("E072A1D9E0CC"), "303a", "831a", "00", None),
            "usb-Nabu_Casa_ZBT-2_E072A1D9E0CC-if00"
        );
        // A CH340 with no manufacturer or serial string.
        assert_eq!(
            by_id_name(None, Some("USB Serial"), None, "1a86", "7523", "00", Some("0")),
            "usb-1a86_USB_Serial-if00-port0"
        );
        assert_eq!(
            by_id_name(Some("Silicon Labs"), Some("Sonoff Zigbee 3.0 USB Dongle Plus"), Some("ba3b0b0b"), "10c4", "ea60", "00", Some("0")),
            "usb-Silicon_Labs_Sonoff_Zigbee_3.0_USB_Dongle_Plus_ba3b0b0b-if00-port0"
        );
        assert_eq!(udev_part("  a  b/c "), "a_b_c");
    }

    #[test]
    fn tty_uevents_are_read_and_others_ignored() {
        let add = b"add@/devices/platform/vhci_hcd.0/usb1/1-1/1-1:1.0/tty/ttyACM0\0ACTION=add\0DEVPATH=/devices/x\0SUBSYSTEM=tty\0DEVNAME=ttyACM0\0";
        assert_eq!(tty_event(add), Some(("add".into(), "ttyACM0".into())));
        let other = b"add@/devices/x\0ACTION=add\0SUBSYSTEM=usb\0DEVNAME=bus/usb/001/002\0";
        assert_eq!(tty_event(other), None);
        let console = b"add@/devices/x\0ACTION=add\0SUBSYSTEM=tty\0DEVNAME=ttyAMA0\0";
        assert_eq!(tty_event(console), None);
    }

    #[test]
    fn the_header_is_read_big_endian() {
        let h = Header::parse(&[0, 1, 0, 2, 0, 0, 0, 2, 0x30, 0x3a, 0x83, 0x1a]);
        assert_eq!(h, Header { devid: 0x0001_0002, speed: 2, vendor: 0x303a, product: 0x831a });
    }
}
