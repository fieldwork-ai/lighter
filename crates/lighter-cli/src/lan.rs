//! The machine on the Mac's network: choosing the card, holding the MAC,
//! and bridging through vmnet in this process, which needs
//! `com.apple.vm.networking`. A release's lighter.app holds it; the ad hoc
//! bundle a checkout builds cannot, and starts without the card.

use std::net::Ipv4Addr;
use std::path::Path;

/// Whether this installation's machine can bridge: a release's lighter.app
/// holds `com.apple.vm.networking`; the ad hoc bundle a checkout builds
/// cannot.
pub fn can_bridge() -> bool {
    crate::paths::guest_dir()
        .map(|guest| guest.join("lighter.app"))
        .is_ok_and(|app| carries_vm_networking(&app))
}

/// Asked of `codesign`, as `doctor` asks of the hypervisor entitlement: an
/// entitlement counts only under a valid signature.
fn carries_vm_networking(path: &Path) -> bool {
    path.exists()
        && std::process::Command::new("/usr/bin/codesign")
            .args(["-d", "--entitlements", "-", "--xml"])
            .arg(path)
            .output()
            .is_ok_and(|out| {
                String::from_utf8_lossy(&out.stdout).contains("com.apple.vm.networking")
            })
}

/// The card to bridge: the one named, or for `auto` the Mac's primary
/// (its default route's), if vmnet can bridge it.
pub fn interface(configured: &str) -> Result<String, String> {
    let bridgeable = lighter_vmnet::interfaces();
    let wanted = if configured.is_empty() || configured == "auto" {
        primary_interface().ok_or("the Mac has no default route to choose a network card by")?
    } else {
        configured.to_string()
    };
    if !bridgeable.contains(&wanted) {
        return Err(format!(
            "{wanted} cannot be bridged (vmnet can bridge: {})",
            if bridgeable.is_empty() {
                "none".into()
            } else {
                bridgeable.join(", ")
            }
        ));
    }
    Ok(wanted)
}

/// The interface the Mac's default route leaves by.
pub fn primary_interface() -> Option<String> {
    let out = std::process::Command::new("/sbin/route")
        .args(["-n", "get", "default"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        l.trim()
            .strip_prefix("interface:")
            .map(|i| i.trim().to_string())
    })
}

/// Whether `interface` is the Mac's Wi-Fi, where the guest's MAC is
/// translated to the Mac's and a router may not give it a lease.
pub fn is_wifi(interface: &str) -> bool {
    let Ok(out) = std::process::Command::new("/usr/sbin/networksetup")
        .arg("-listallhardwareports")
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut wifi = false;
    for line in text.lines() {
        if let Some(port) = line.strip_prefix("Hardware Port: ") {
            wifi = port.contains("Wi-Fi") || port.contains("AirPort");
        } else if let Some(device) = line.strip_prefix("Device: ")
            && device.trim() == interface
        {
            return wifi;
        }
    }
    false
}

/// The Mac's IPv4 address and prefix on `interface`.
pub fn mac_address(interface: &str) -> Option<(Ipv4Addr, u8)> {
    let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills the list we free below.
    if unsafe { libc::getifaddrs(&mut addrs) } != 0 {
        return None;
    }
    let mut found = None;
    let mut cur = addrs;
    while !cur.is_null() {
        // SAFETY: a node of the list getifaddrs returned.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() || ifa.ifa_netmask.is_null() {
            continue;
        }
        // SAFETY: a NUL-terminated name.
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }.to_string_lossy();
        // SAFETY: the family is read from a valid sockaddr.
        if name != interface || i32::from(unsafe { (*ifa.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        // SAFETY: AF_INET, so both are sockaddr_in.
        let (addr, mask) = unsafe {
            (
                (*(ifa.ifa_addr as *const libc::sockaddr_in))
                    .sin_addr
                    .s_addr,
                (*(ifa.ifa_netmask as *const libc::sockaddr_in))
                    .sin_addr
                    .s_addr,
            )
        };
        found = Some((
            Ipv4Addr::from(u32::from_be(addr)),
            u32::from_be(mask).count_ones() as u8,
        ));
        break;
    }
    // SAFETY: the list getifaddrs gave us.
    unsafe { libc::freeifaddrs(addrs) };
    found
}

/// The guest's address as the kernel command line gives it: `dhcp`, or an
/// address with its prefix (the Mac's interface's, when none is named).
pub fn address_for_guest(configured: &str, interface: &str) -> Result<String, String> {
    if configured.is_empty() || configured == "auto" {
        return Ok("dhcp".into());
    }
    let (addr, prefix) = match configured.split_once('/') {
        Some((a, p)) => (
            a,
            Some(
                p.parse::<u8>()
                    .map_err(|_| format!("bad prefix in {configured}"))?,
            ),
        ),
        None => (configured, None),
    };
    let addr: Ipv4Addr = addr
        .parse()
        .map_err(|_| format!("{configured} is not an IPv4 address"))?;
    let prefix = match prefix {
        Some(p) if (8..=30).contains(&p) => p,
        Some(p) => return Err(format!("a /{p} network is not one a LAN address is on")),
        None => mac_address(interface).map(|(_, p)| p).unwrap_or(24),
    };
    Ok(format!("{addr}/{prefix}"))
}

/// The card's MAC, kept in the machine's home so a router's lease, and
/// any reservation made for it, survive restarts.
pub fn mac(home: &Path) -> std::io::Result<[u8; 6]> {
    let path = home.join("lan-mac");
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Some(mac) = lighter_vmnet::parse_mac(text.trim())
    {
        return Ok(mac);
    }
    let mac = lighter_vmm::lan::random_mac();
    std::fs::write(&path, lighter_vmnet::mac_text(mac) + "\n")?;
    Ok(mac)
}

/// Bridges `interface` in this process.
pub fn connect(interface: &str, mac: [u8; 6]) -> Result<lighter_vmm::lan::Lan, String> {
    lighter_vmm::lan::Lan::in_process(interface, mac).map_err(|refused| {
        if std::env::current_exe().is_ok_and(|exe| carries_vm_networking(&exe)) {
            format!("vmnet would not bridge {interface}: {refused}")
        } else {
            "this lighter cannot put the machine on the network: it needs com.apple.vm.networking, which a release holds and a build of your own cannot".into()
        }
    })
}

/// `lighter lan status`.
pub fn status() -> anyhow::Result<std::process::ExitCode> {
    if !can_bridge() {
        println!(
            "  bridging   not possible: this lighter lacks com.apple.vm.networking, which a release holds"
        );
    }
    let interfaces = lighter_vmnet::interfaces();
    println!(
        "  bridgeable {}",
        if interfaces.is_empty() {
            "none".to_string()
        } else {
            interfaces.join(", ")
        }
    );
    match primary_interface() {
        Some(primary) => println!(
            "  primary    {primary}{}",
            if is_wifi(&primary) {
                " (Wi-Fi: set an address with `lighter config --lan-address` unless your router leases one)"
            } else {
                ""
            }
        ),
        None => println!("  primary    none"),
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_static_address_takes_a_prefix() {
        assert_eq!(address_for_guest("", "en0").unwrap(), "dhcp");
        assert_eq!(address_for_guest("auto", "en0").unwrap(), "dhcp");
        assert_eq!(
            address_for_guest("192.168.50.240/24", "en0").unwrap(),
            "192.168.50.240/24"
        );
        assert!(address_for_guest("192.168.50.240/31", "en0").is_err());
        assert!(address_for_guest("fd00::1", "en0").is_err());
        assert!(
            address_for_guest("192.168.50.240", "nonexistent0")
                .unwrap()
                .ends_with("/24")
        );
    }

    #[test]
    fn the_mac_is_kept() {
        let home = std::env::temp_dir().join(format!("lighter-lan-mac-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let first = mac(&home).unwrap();
        assert_eq!(mac(&home).unwrap(), first);
        std::fs::remove_dir_all(home).unwrap();
    }
}
