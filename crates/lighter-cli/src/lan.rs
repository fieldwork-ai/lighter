//! The machine on the Mac's network: choosing the card, holding the MAC,
//! bridging through vmnet in this process or through the root helper, and
//! installing that helper (`sudo lighter lan enable`).

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

const LABEL: &str = "dev.lighter.bridge";
const HELPER: &str = "/Library/PrivilegedHelperTools/dev.lighter.bridge";
const PLIST: &str = "/Library/LaunchDaemons/dev.lighter.bridge.plist";
const LOG: &str = "/var/log/dev.lighter.bridge.log";
/// What an installed helper must be: Fieldwork's, notarized.
const HELPER_REQUIREMENT: &str =
    "anchor apple generic and certificate leaf[subject.OU] = \"N7N6BNF95K\"";

/// The helper's socket: the installed one's, or `LIGHTER_BRIDGE_SOCKET`
/// for a helper run by hand (`lighter-bridge --socket … --any-client`).
pub fn socket() -> PathBuf {
    std::env::var_os("LIGHTER_BRIDGE_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(lighter_vmnet::helper::SOCKET))
}

/// Whether the helper is installed.
pub fn installed() -> bool {
    Path::new(PLIST).exists() && Path::new(HELPER).exists()
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
        && let Some(mac) = lighter_vmnet::helper::parse_mac(text.trim())
    {
        return Ok(mac);
    }
    let mac = lighter_vmm::lan::random_mac();
    std::fs::write(&path, lighter_vmnet::helper::mac_text(mac) + "\n")?;
    Ok(mac)
}

/// Bridges `interface`: in this process when vmnet allows it (root, or
/// lighter with `com.apple.vm.networking`), through the helper otherwise.
pub fn connect(
    interface: &str,
    mac: [u8; 6],
) -> Result<(lighter_vmm::lan::Lan, &'static str), String> {
    if let Ok(lan) = lighter_vmm::lan::Lan::in_process(interface, mac) {
        return Ok((lan, "in process"));
    }
    let socket = socket();
    if !socket.exists() {
        return Err(
            "lighter's network helper is not installed: run `sudo lighter lan enable`".into(),
        );
    }
    lighter_vmm::lan::Lan::via_helper(&socket, interface, mac)
        .map(|lan| (lan, "through the helper"))
        .map_err(|e| format!("the network helper refused: {e}"))
}

/// `sudo lighter lan enable`: installs the helper as a launchd daemon that
/// serves the user who ran sudo (and any earlier ones).
pub fn enable() -> anyhow::Result<std::process::ExitCode> {
    // SAFETY: plain geteuid.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("lighter: run it with sudo: `sudo lighter lan enable`");
        return Ok(std::process::ExitCode::FAILURE);
    }
    let uid: u32 = match std::env::var("SUDO_UID").ok().and_then(|u| u.parse().ok()) {
        Some(uid) if uid != 0 => uid,
        _ => {
            eprintln!(
                "lighter: run it with sudo from the account that uses lighter, so the helper knows whom to serve"
            );
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    let source = bundled_helper()
        .ok_or_else(|| anyhow::anyhow!("this installation has no lighter-bridge beside it"))?;
    let verified = std::process::Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", &format!("-R={HELPER_REQUIREMENT}")])
        .arg(&source)
        .status()?;
    if !verified.success() {
        eprintln!(
            "lighter: {} is not signed by Fieldwork, so it is not installed as root. A build of your own runs by hand: `sudo {} --socket /tmp/lighter-bridge.sock --allow-uid {uid} --any-client`, with LIGHTER_BRIDGE_SOCKET=/tmp/lighter-bridge.sock",
            source.display(),
            source.display()
        );
        return Ok(std::process::ExitCode::FAILURE);
    }
    let mut uids = allowed_uids();
    if !uids.contains(&uid) {
        uids.push(uid);
    }
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["bootout", &format!("system/{LABEL}")])
        .status();
    std::fs::create_dir_all("/Library/PrivilegedHelperTools")?;
    let staging = format!("{HELPER}.new");
    std::fs::copy(&source, &staging)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
    let c = std::ffi::CString::new(staging.as_str())?;
    // SAFETY: chown of a path we just wrote, to root:wheel.
    unsafe { libc::chown(c.as_ptr(), 0, 0) };
    std::fs::rename(&staging, HELPER)?;
    std::fs::write(PLIST, plist(&uids))?;
    std::fs::set_permissions(PLIST, std::fs::Permissions::from_mode(0o644))?;
    let loaded = std::process::Command::new("/bin/launchctl")
        .args(["bootstrap", "system", PLIST])
        .status()?;
    if !loaded.success() {
        anyhow::bail!("launchctl could not load {PLIST}");
    }
    println!(
        "lighter's network helper is installed, for user {}.",
        uids.iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("Turn LAN mode on with `lighter config --lan on` and `lighter restart`.");
    Ok(std::process::ExitCode::SUCCESS)
}

/// `sudo lighter lan disable`: removes the helper.
pub fn disable() -> anyhow::Result<std::process::ExitCode> {
    // SAFETY: plain geteuid.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("lighter: run it with sudo: `sudo lighter lan disable`");
        return Ok(std::process::ExitCode::FAILURE);
    }
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["bootout", &format!("system/{LABEL}")])
        .status();
    for path in [PLIST, HELPER, lighter_vmnet::helper::SOCKET] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    println!("lighter's network helper is removed. Machines with LAN mode on start without it.");
    Ok(std::process::ExitCode::SUCCESS)
}

/// `lighter lan status`.
pub fn status() -> anyhow::Result<std::process::ExitCode> {
    if installed() {
        let uids = allowed_uids();
        println!(
            "  helper     installed ({HELPER}), serving user {}",
            uids.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    } else {
        println!("  helper     not installed (`sudo lighter lan enable`)");
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

/// The helper shipped with this lighter: beside the binary in a checkout,
/// in `share/lighter` in an installation.
fn bundled_helper() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let dir = exe.parent()?;
    [
        dir.join("lighter-bridge"),
        dir.join("../share/lighter/lighter-bridge"),
    ]
    .into_iter()
    .find(|p| p.exists())
}

/// The users the installed helper serves, from its plist.
fn allowed_uids() -> Vec<u32> {
    let Ok(text) = std::fs::read_to_string(PLIST) else {
        return Vec::new();
    };
    let strings: Vec<&str> = text
        .split("<string>")
        .skip(1)
        .filter_map(|s| s.split("</string>").next())
        .collect();
    strings
        .windows(2)
        .filter(|w| w[0] == "--allow-uid")
        .filter_map(|w| w[1].parse().ok())
        .collect()
}

fn plist(uids: &[u32]) -> String {
    let args: String = uids
        .iter()
        .map(|u| format!("\n\t\t<string>--allow-uid</string>\n\t\t<string>{u}</string>"))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{HELPER}</string>{args}
	</array>
	<key>Sockets</key>
	<dict>
		<key>Listeners</key>
		<dict>
			<key>SockPathName</key>
			<string>{socket}</string>
			<key>SockPathMode</key>
			<integer>438</integer>
		</dict>
	</dict>
	<key>StandardErrorPath</key>
	<string>{LOG}</string>
</dict>
</plist>
"#,
        socket = lighter_vmnet::helper::SOCKET
    )
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
    fn the_plist_names_its_users_and_the_socket() {
        let text = plist(&[501, 502]);
        assert!(text.contains("<string>--allow-uid</string>\n\t\t<string>501</string>"));
        assert!(
            text.contains("<integer>438</integer>"),
            "0666: the helper checks who connects itself"
        );
        assert!(text.contains(lighter_vmnet::helper::SOCKET));
        assert!(!text.contains("--any-client"));
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
