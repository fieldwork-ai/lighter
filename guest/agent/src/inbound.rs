//! Where an inbound stream is dialled when Docker refuses the address the
//! host named.
//!
//! Docker publishes a port twice, on `0.0.0.0` and on `::`, and the host
//! dials each into this guest on its own family. The v6 one reaches the
//! container's IPv6 address, where most servers do not listen: they bind
//! `0.0.0.0`, and Docker refuses the v6 connection on their behalf. On a
//! Mac `localhost` is `::1` first, so that refusal read as "the server does
//! not work on localhost". A refused v6 destination is retried on the IPv4
//! address of the interface that holds the v6 one, which is Docker's v4
//! mapping of the same port. Only on refusal: a server that answers over
//! v6 is reached over v6 as before.

use std::net::{IpAddr, SocketAddr};

/// The v4 sibling of a refused v6 destination: the same port on the IPv4
/// address of the interface that holds the v6 one. None for a v4
/// destination, an address no interface holds, or an interface with no v4.
pub fn v4_sibling(dst: SocketAddr, interfaces: &[(String, IpAddr)]) -> Option<SocketAddr> {
    let IpAddr::V6(v6) = dst.ip() else {
        return None;
    };
    let name = interfaces
        .iter()
        .find(|(_, ip)| *ip == IpAddr::V6(v6))?
        .0
        .as_str();
    let v4 = interfaces.iter().find_map(|(n, ip)| match ip {
        IpAddr::V4(a) if n == name => Some(*a),
        _ => None,
    })?;
    Some(SocketAddr::new(v4.into(), dst.port()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn guest() -> Vec<(String, IpAddr)> {
        vec![
            ("lo".into(), Ipv4Addr::LOCALHOST.into()),
            ("lo".into(), Ipv6Addr::LOCALHOST.into()),
            ("eth0".into(), Ipv4Addr::new(192, 168, 127, 2).into()),
            (
                "eth0".into(),
                Ipv6Addr::new(0xfd6c, 0x6967, 0x6874, 0, 0, 0, 0, 2).into(),
            ),
            ("docker0".into(), Ipv4Addr::new(172, 17, 0, 1).into()),
        ]
    }

    #[test]
    fn a_refused_v6_publish_is_retried_on_the_same_interfaces_v4() {
        let dst = SocketAddr::new(
            Ipv6Addr::new(0xfd6c, 0x6967, 0x6874, 0, 0, 0, 0, 2).into(),
            8080,
        );
        assert_eq!(
            v4_sibling(dst, &guest()),
            Some(SocketAddr::new(
                Ipv4Addr::new(192, 168, 127, 2).into(),
                8080
            ))
        );
        let lo = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 8080);
        assert_eq!(
            v4_sibling(lo, &guest()),
            Some(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 8080))
        );
    }

    #[test]
    fn nothing_for_a_v4_destination_or_an_address_that_is_not_ours() {
        assert_eq!(
            v4_sibling(
                SocketAddr::new(Ipv4Addr::new(192, 168, 127, 2).into(), 80),
                &guest()
            ),
            None
        );
        let foreign = SocketAddr::new(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).into(), 80);
        assert_eq!(v4_sibling(foreign, &guest()), None);
    }

    #[test]
    fn nothing_when_the_interface_has_no_v4() {
        let v6_only = vec![(
            "eth1".to_string(),
            IpAddr::from(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 9)),
        )];
        let dst = SocketAddr::new(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 9).into(), 443);
        assert_eq!(v4_sibling(dst, &v6_only), None);
    }
}
