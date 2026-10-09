# Direct access: the Mac reaches containers at their own addresses (0.13)

9 October 2026. The networking head to head of the same day (`networking-head-to-head-2026-10-09.md`) found one thing OrbStack does that lighter did not: the Mac reaching a container by its IP and by name (`web.orb.local`), without admin rights. This is how lighter does it, and why each piece is the way it is.

## The link

A third network card, on a vmnet **host-mode** network: a network of the Mac and the machine alone. vmnet gives the Mac an interface (`bridge100` and on) at the address it is asked for (`vmnet_host_ip_address_key`, `vmnet_host_subnet_mask_key`), with the route to the whole subnet through it. It needs `com.apple.vm.networking` and no root, so it runs in the machine's own process (`lighter_vmnet::Bridge::host_link`, `lighter_vmm::lan::Lan::host_link`); there is no helper fallback, and a build without the entitlement starts without it, saying why in `lighter status` and `lighter doctor`.

- **One /16, `S`:** the Mac is `S.0.1/16`, the guest's card `link0` is `S.0.2/24`, and Docker's networks are made inside `S`: `--bip S.1.1/24 --default-address-pool base=S/16,size=24`. The Mac routes all of `S` to the link; the guest answers the Mac's ARP for any address it routes to one of Docker's bridges (`proxy_arp` on `link0`), so the Mac's packets for a container arrive at the machine and are forwarded. No route on the Mac per network, which is what would need root.
- **The rest of `S` is unreachable in the guest** (`ip route add unreachable S/16`; the connected /24s are more specific). Without it an unused address was proxied too, and a connection to it left by `eth0` as a stream to the Mac, which routed it back to the link.
- **Docker lets the Mac in:** `--allow-direct-routing` (Docker 28 otherwise drops traffic for a container from outside its bridge in the raw table) and `iptables -I DOCKER-USER -i link0 -j ACCEPT` (its filter otherwise drops it for an unpublished port). The container sees the Mac's address, `S.0.1`.
- **The streams are untouched:** their rules match `fib daddr oifname { eth0, eth1 }`, and nothing on the link routes that way.
- **Found by MAC:** the guest is told `lighter.link=S/16,<mac>` and renames whichever card has that MAC to `link0`, since its number depends on whether LAN mode's card is there.

## Choosing `S`

The first of `10.211/16` … `10.250/16` that overlaps no route the Mac has (`netstat -rn -f inet`), kept in `LIGHTER_HOME/link` with the card's MAC and the network's identifier, because containers' addresses come from it. Any overlapping route is a clash, including another lighter's link (a gate's machine beside the daily one gets the next range); a bridge on exactly `S` gets three seconds to go, which is a restart's own link going down. A kept subnet that now clashes is refused with the route named, and `lighter config --direct-subnet` chooses another.

## Names

`web.lighter.local` for a container named `web`, `<service>.<project>.lighter.local` for a Compose service (every replica's address), a host-network container at `S.0.2`. Only addresses inside `S` are named. A watcher on Docker's container and network events (`lighter_docker::names`) hands the whole set to the link card.

**The link card answers the Mac's multicast DNS itself** (`lighter_vmm::mdns`), as the first card answers ARP and DHCP: the Mac asks every network it is on for a `.local` name, the link among them, and the card answers before the guest sees the question. The first design registered the names with mDNSResponder as local-only records (`DNSServiceRegisterRecord` on `kDNSServiceInterfaceIndexLocalOnly`). They resolved, but `getaddrinfo` asks for IPv6 too, nothing answered that, and every lookup waited out mDNS's five seconds; a local-only NSEC record did not help. Answered on the link, the NSEC does, in exactly the form macOS uses for its own name, which was found by capturing it: the NSEC **in the additional section, never as an answer**, its next name a **pointer to its own name**, and its bitmap the types the name has (A) without NSEC itself. The first form differed in all three (the NSEC as an answer, its next name written out, NSEC in its bitmap), and mDNSResponder took the A and ignored the negative, so a lookup took two seconds; which of the three it objects to was not narrowed down. In macOS's form `dns-sd` reports "No Such Record" for IPv6 at once and a lookup takes 0.03 to 0.16 seconds. A question asking for a unicast reply (QU) gets one, as RFC 6762 allows. A change is announced as it happens: a goodbye (TTL 0) for an address a name no longer has, and a name's new addresses, so a removed container's name stops resolving within a quarter of a second instead of when the Mac's cached copy expires (ten seconds). A name nobody holds gets no answer, because another lighter's link may hold it.

Containers resolve the names too: their DNS is the Mac's resolver.

## Isolation

Nothing but the Mac is on the link. Another device has no route to `S` (checked from the M1: its route is the router's), and the names are answered only on the link, so its own query gets nothing.

## Limits

- Networks made before 0.13, or with a subnet of their own, are outside `S`; `lighter status` and `doctor` list them. Recreating a Compose project puts it inside.
- IPv4 only. Containers' IPv6 still leaves through the streams.
- macOS's Local Network privacy covers the link as it covers any local network: an app it has not asked about gets `EHOSTUNREACH` (Homebrew's Python did), and Apple's own tools are exempt.
- An address in `S` that nothing has times out from the Mac (no ARP reply) rather than failing at once.

## Also in 0.13

**traceroute** (`crates/lighter-vmm/src/net.rs`, the hops section). UDP with a TTL under 31 skips the streams (init's divert rule) and reaches the card as packets; the card is a router hop: time exceeded from the gateway for a packet with nothing left, otherwise sent on from the Mac one TTL shorter (a UDP socket for probes, the ICMP socket for echo), and the errors macOS hands every unprivileged ICMP socket (checked: time exceeded for both echo and UDP probes, IP header included) turned into the errors the guest would have had, quoting what it sent, so its connection tracking passes them to the container. Full-TTL UDP reaching the card is still dropped: it means something escaped the streams.

**LAN mode's forward chain** accepts what answers a container's own packets (`ct state established,related`); a bridge container's ping to a LAN device left by `eth1` and its reply was dropped.

**localhost for host-network containers** (`guest/agent/src/loopback.rs`, `listeners.rs`). Init redirects a connection to `127.0.0.0/8` or `::1` to the agent, and so to the Mac's loopback, on a port in no guest listener's set (`local_tcp`, kept by the agent from the same scan as the host-network listeners, every port until its first look). Its SYN first waits in `nft queue 7` while the agent asks the VMM, over the DNS stream, `<port>.tcp.loopback.lighter.internal`; the VMM answers from a bind of that loopback address without `SO_REUSEADDR` (in use means something holds it: checked for listeners on the address, the wildcard and a dual-stack v6 socket), and a no is refused with a reset by a mark on the requeued SYN. Without that, the agent accepted every such connection and the Mac's refusal arrived as a reset after it, which `nc -z` reads as open. The bind holds the address for microseconds; a Mac service binding in that same instant would be refused once.

**Every DNS type from the Mac's resolver** (`crates/lighter-vmm/src/sysdns.rs`): `DNSServiceQueryRecord`, the resolver `getaddrinfo` uses, for everything that is not A or AAAA, each record under its own name; a raw forward to `resolv.conf`'s first nameserver only when the resolver cannot say. One loss: the resolver reports a missing name and a name without that type alike, so both answer NOERROR with no records.
