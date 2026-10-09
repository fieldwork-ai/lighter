//! The link: a network of the Mac and the machine alone, through which the
//! Mac reaches a container at its own address (`10.211.1.5`) and by name
//! (`web.lighter.local`), with no port published.
//!
//! It is a vmnet host-only network (`lighter_vmm::lan::Lan::host_link`): the
//! Mac gets an interface (`bridge100`) at `S.0.1` on a /16 `S`, with the
//! route to all of `S` through it. Docker's networks are made inside `S`
//! (`--bip S.1.1/24`, `--default-address-pool base=S/16,size=24`), and the
//! guest answers the Mac's ARP for any of their addresses (proxy ARP on
//! `link0`), so the Mac's packets for a container arrive at the machine and
//! are routed to it. The link card answers the Mac's multicast DNS
//! questions for containers' names (`lighter_vmm::mdns`). Nothing but the Mac is on that network, so nothing else
//! reaches a container this way. Starting it needs `com.apple.vm.networking`
//! and no root; a build without the entitlement starts without it.
//!
//! `S` is chosen once and kept (`LIGHTER_HOME/link`), because the
//! containers' addresses come from it: a subnet the Mac already routes
//! somewhere (a VPN's, a LAN's) is passed over when choosing, and refused
//! with a reason when it later becomes one.

use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;

/// The subnets tried, in order, when none is chosen: inside 10/8, which
/// home networks seldom use, at second octets few VPNs pick.
const CANDIDATES: std::ops::RangeInclusive<u8> = 211..=250;

/// What the link is: kept, so that a restart rejoins the same network with
/// the same addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    /// The /16, as its first address (`10.211.0.0`).
    pub subnet: Ipv4Addr,
    pub mac: [u8; 6],
    pub network: [u8; 16],
}

impl Link {
    /// The Mac's address on the link.
    pub fn host(&self) -> Ipv4Addr {
        let [a, b, ..] = self.subnet.octets();
        Ipv4Addr::new(a, b, 0, 1)
    }

    /// The machine's.
    pub fn guest(&self) -> Ipv4Addr {
        let [a, b, ..] = self.subnet.octets();
        Ipv4Addr::new(a, b, 0, 2)
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        ip.octets()[..2] == self.subnet.octets()[..2]
    }

    /// What the guest is told: the subnet and the card's MAC, by which it
    /// finds the card whatever number it got.
    pub fn kernel_arg(&self) -> String {
        format!(
            " lighter.link={}/16,{}",
            self.subnet,
            lighter_vmnet::helper::mac_text(self.mac)
        )
    }

    fn text(&self) -> String {
        format!(
            "{}/16 {} {}\n",
            self.subnet,
            lighter_vmnet::helper::mac_text(self.mac),
            self.network
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    }

    fn parse(text: &str) -> Option<Link> {
        let mut words = text.split_whitespace();
        let subnet = parse_subnet(words.next()?).ok()?;
        let mac = lighter_vmnet::helper::parse_mac(words.next()?)?;
        let hex = words.next()?;
        if hex.len() != 32 {
            return None;
        }
        let mut network = [0u8; 16];
        for (i, byte) in network.iter_mut().enumerate() {
            *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(Link {
            subnet,
            mac,
            network,
        })
    }
}

/// `10.211.0.0/16` (or `10.211.0.0`, or `10.211`) as the /16's first
/// address; anything that is not a private /16 is refused.
pub fn parse_subnet(text: &str) -> Result<Ipv4Addr, String> {
    let bad = || format!("{text} is not a /16 such as 10.211.0.0/16");
    let (address, prefix) = text.split_once('/').unwrap_or((text, "16"));
    if prefix != "16" {
        return Err(format!(
            "{text}: the link is a /16 (Docker's networks are made inside it)"
        ));
    }
    let octets: Vec<u8> = address
        .split('.')
        .map(|o| o.parse().map_err(|_| bad()))
        .collect::<Result<_, _>>()?;
    let (a, b) = match octets.as_slice() {
        [a, b] | [a, b, 0, 0] => (*a, *b),
        _ => return Err(bad()),
    };
    let ip = Ipv4Addr::new(a, b, 0, 0);
    if !ip.is_private() {
        return Err(format!(
            "{text} is not a private range (10/8, 172.16/12, 192.168/16)"
        ));
    }
    Ok(ip)
}

/// The link to start: the one kept, else a new one on `configured` or the
/// first candidate the Mac does not route already. A kept or configured
/// subnet the Mac now routes elsewhere is refused, naming the route.
pub fn plan(home: &Path, configured: &str) -> Result<Link, String> {
    let path = home.join("link");
    let kept = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| Link::parse(&t));
    let routes = routes();
    let wanted = if configured.is_empty() || configured == "auto" {
        None
    } else {
        Some(parse_subnet(configured)?)
    };
    let link = match (kept, wanted) {
        (Some(kept), None) => kept,
        (Some(kept), Some(subnet)) if kept.subnet == subnet => kept,
        (kept, wanted) => {
            let subnet = match wanted {
                Some(subnet) => subnet,
                None => CANDIDATES
                    .map(|b| Ipv4Addr::new(10, b, 0, 0))
                    .find(|s| clash(&routes, *s).is_none())
                    .ok_or("every subnet tried for the link is routed by the Mac already")?,
            };
            let link = Link {
                subnet,
                mac: kept.map_or_else(lighter_vmm::lan::random_mac, |k| k.mac),
                network: kept.map_or_else(random_network, |k| k.network),
            };
            std::fs::write(&path, link.text())
                .map_err(|e| format!("cannot keep the link's settings: {e}"))?;
            link
        }
    };
    // A machine restarting finds its own last link's interface for a moment
    // after the old process is gone: vmnet takes it down shortly after.
    let mut routes = routes;
    for _ in 0..30 {
        match clash(&routes, link.subnet) {
            Some(r) if r.prefix == 16 && r.interface.starts_with("bridge") => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                routes = self::routes();
            }
            _ => break,
        }
    }
    if let Some(route) = clash(&routes, link.subnet) {
        return Err(format!(
            "the Mac already routes {} through {} ({}), which the link's {}/16 overlaps; choose another with `lighter config --direct-subnet`",
            route.text,
            route.interface,
            route.describe(),
            link.subnet
        ));
    }
    Ok(link)
}

fn random_network() -> [u8; 16] {
    let mut id = [0u8; 16];
    // SAFETY: arc4random_buf fills the buffer it is given.
    unsafe { libc::arc4random_buf(id.as_mut_ptr().cast(), id.len()) };
    // A version 4 UUID, as vmnet's identifiers are.
    id[6] = (id[6] & 0x0f) | 0x40;
    id[8] = (id[8] & 0x3f) | 0x80;
    id
}

/// A route in the Mac's table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Route {
    network: Ipv4Addr,
    prefix: u8,
    interface: String,
    text: String,
}

impl Route {
    fn describe(&self) -> &'static str {
        if self.interface.starts_with("utun") || self.interface.starts_with("ipsec") {
            "a VPN"
        } else if self.interface.starts_with("bridge") {
            "another virtual machine's network"
        } else {
            "a network the Mac is on"
        }
    }
}

/// The first route that overlaps `subnet`/16.
fn clash(routes: &[Route], subnet: Ipv4Addr) -> Option<&Route> {
    routes.iter().find(|r| {
        let shorter = r.prefix.min(16) as u32;
        let mask = |ip: Ipv4Addr| u32::from(ip) & u32::MAX.checked_shl(32 - shorter).unwrap_or(0);
        mask(r.network) == mask(subnet)
    })
}

/// The Mac's IPv4 routes, from `netstat`, without the default routes and
/// the multicast and broadcast ones, which overlap everything or nothing.
fn routes() -> Vec<Route> {
    std::process::Command::new("/usr/sbin/netstat")
        .args(["-rn", "-f", "inet"])
        .output()
        .map(|out| parse_routes(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or_default()
}

fn parse_routes(table: &str) -> Vec<Route> {
    table
        .lines()
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let (destination, interface) = (*words.first()?, *words.get(3)?);
            let (address, prefix) = match destination.split_once('/') {
                Some((a, p)) => (a, Some(p.parse::<u8>().ok()?)),
                None => (destination, None),
            };
            let octets: Vec<u8> = address
                .split('.')
                .map(|o| o.parse().ok())
                .collect::<Option<_>>()?;
            if octets.is_empty() || octets.len() > 4 {
                return None;
            }
            // netstat leaves off a network's trailing zero octets, and the
            // prefix when it is those octets' length: `192.168.50` is a /24.
            let prefix = prefix.unwrap_or(octets.len() as u8 * 8).min(32);
            let mut full = [0u8; 4];
            full[..octets.len()].copy_from_slice(&octets);
            let network = Ipv4Addr::from(full);
            if network.is_multicast() || network.is_broadcast() || network.is_loopback() {
                return None;
            }
            Some(Route {
                network,
                prefix,
                interface: interface.to_string(),
                text: destination.to_string(),
            })
        })
        .collect()
}

/// Starts the link's card, in this process: the entitlement or nothing.
/// It answers the Mac's questions about the names in `names`.
pub fn connect(
    link: &Link,
    names: Arc<lighter_vmm::mdns::Names>,
) -> Result<lighter_vmm::lan::Lan, String> {
    lighter_vmm::lan::Lan::host_link(
        link.host(),
        Ipv4Addr::new(255, 255, 0, 0),
        link.network,
        link.mac,
        names,
    )
    .map_err(|why| {
        if why.contains("refused") {
            "this lighter cannot make the link (a release can: it holds com.apple.vm.networking)"
                .to_string()
        } else {
            why
        }
    })
}

/// The state of direct access on a running machine: the link's subnet, or
/// why it started without one.
pub fn state(home: &Path) -> Result<Ipv4Addr, String> {
    if let Ok(why) = std::fs::read_to_string(home.join("link-error")) {
        return Err(why.trim().to_string());
    }
    std::fs::read_to_string(home.join("link"))
        .ok()
        .and_then(|t| Link::parse(&t))
        .map(|l| l.subnet)
        .ok_or_else(|| "the machine has not made it yet".to_string())
}

/// Docker's bridge networks whose addresses are outside `subnet`, made
/// before direct access or given a subnet of their own: the Mac reaches
/// their containers by published ports only.
pub fn networks_outside(socket: &Path, subnet: Ipv4Addr) -> Vec<String> {
    let Ok(networks) = lighter_docker::http::get_json(socket, "/networks") else {
        return Vec::new();
    };
    let prefix = subnet.octets();
    let mut outside: Vec<String> = networks
        .as_array()
        .into_iter()
        .flatten()
        .filter(|n| n.get("Driver").and_then(|d| d.as_str()) == Some("bridge"))
        .filter(|n| {
            n.pointer("/IPAM/Config")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
                .filter_map(|c| {
                    c.get("Subnet")?
                        .as_str()?
                        .split('/')
                        .next()?
                        .parse::<Ipv4Addr>()
                        .ok()
                })
                .any(|net| net.octets()[..2] != prefix[..2])
        })
        .filter_map(|n| Some(n.get("Name")?.as_str()?.to_string()))
        .collect();
    outside.sort();
    outside
}

/// Docker's names, into the link's answers.
pub struct Names(pub Arc<lighter_vmm::mdns::Names>);

impl lighter_docker::names::NameSink for Names {
    fn set(&self, records: lighter_docker::names::Records) {
        self.0.set(records);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "Routing tables

Internet:
Destination        Gateway            Flags               Netif Expire
default            192.168.50.1       UGScg                 en1
default            link#41            UCSIg               utun7
10.20/16           link#41            UCS                 utun7
100.64/10          link#41            UCS                 utun7
100.99.112.8       100.99.112.8       UH                  utun7
127                127.0.0.1          UCS                   lo0
169.254            link#25            UCS                   en1      !
192.168.50         link#25            UCS                   en1      !
192.168.50.25/32   link#25            UCS                   en1      !
10.211/16          link#60            UC               bridge100
224.0.0/4          link#25            UmCS                  en1      !
255.255.255.255/32 link#25            UCS                   en1      !
";

    #[test]
    fn netstat_abbreviations_read_as_networks() {
        let routes = parse_routes(TABLE);
        let find = |text: &str| routes.iter().find(|r| r.text == text).unwrap();
        assert_eq!(
            (find("10.20/16").network, find("10.20/16").prefix),
            (Ipv4Addr::new(10, 20, 0, 0), 16)
        );
        assert_eq!(find("192.168.50").prefix, 24);
        assert_eq!(find("169.254").prefix, 16);
        assert_eq!(find("100.99.112.8").prefix, 32);
        assert!(
            routes
                .iter()
                .all(|r| r.text != "default" && r.text != "224.0.0/4" && r.text != "127")
        );
    }

    #[test]
    fn a_route_already_there_is_a_clash() {
        let routes = parse_routes(TABLE);
        let clash_of = |a, b| clash(&routes, Ipv4Addr::new(a, b, 0, 0)).map(|r| r.text.clone());
        assert_eq!(clash_of(10, 20).as_deref(), Some("10.20/16"), "a VPN's");
        assert_eq!(
            clash_of(10, 211).as_deref(),
            Some("10.211/16"),
            "another machine's link"
        );
        assert_eq!(clash_of(10, 212), None);
        // A VPN's /8 covers every candidate.
        let wide = parse_routes("10                 link#9             UCS                utun3\n");
        assert_eq!(wide[0].prefix, 8);
        assert!(clash(&wide, Ipv4Addr::new(10, 211, 0, 0)).is_some());
    }

    #[test]
    fn a_link_survives_its_file() {
        let link = Link {
            subnet: Ipv4Addr::new(10, 211, 0, 0),
            mac: [2, 1, 2, 3, 4, 5],
            network: random_network(),
        };
        assert_eq!(Link::parse(&link.text()), Some(link));
        assert_eq!(link.host(), Ipv4Addr::new(10, 211, 0, 1));
        assert_eq!(link.guest(), Ipv4Addr::new(10, 211, 0, 2));
        assert!(link.contains(Ipv4Addr::new(10, 211, 7, 9)));
        assert!(!link.contains(Ipv4Addr::new(10, 212, 0, 1)));
    }

    #[test]
    fn subnets_are_private_sixteens() {
        assert_eq!(
            parse_subnet("10.211.0.0/16"),
            Ok(Ipv4Addr::new(10, 211, 0, 0))
        );
        assert_eq!(parse_subnet("172.30"), Ok(Ipv4Addr::new(172, 30, 0, 0)));
        assert!(parse_subnet("10.211.0.0/24").is_err());
        assert!(parse_subnet("8.8.0.0/16").is_err());
        assert!(parse_subnet("10.211.1.0/16").is_err());
    }

    #[test]
    fn a_kept_link_is_reused_and_a_new_one_kept() {
        let home = tempfile::tempdir().unwrap();
        let first = plan(home.path(), "auto").unwrap();
        assert_eq!(plan(home.path(), "").unwrap(), first);
        // A subnet chosen later keeps the card and the network's identity.
        let moved = plan(home.path(), "10.249.0.0/16").unwrap();
        assert_eq!(moved.subnet, Ipv4Addr::new(10, 249, 0, 0));
        assert_eq!((moved.mac, moved.network), (first.mac, first.network));
    }
}
