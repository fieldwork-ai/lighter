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
- IPv6 only for the default bridge and networks made with IPv6.
- macOS's Local Network privacy covers the link as it covers any local network: an app it has not asked about gets `EHOSTUNREACH` (Homebrew's Python did), and Apple's own tools are exempt.
- An address in `S` that nothing has times out from the Mac (no ARP reply) rather than failing at once.

## Also in 0.13

**traceroute** (`crates/lighter-vmm/src/net.rs`, the hops section). UDP with a TTL under 31 skips the streams (init's divert rule) and reaches the card as packets; the card is a router hop: time exceeded from the gateway for a packet with nothing left, otherwise sent on from the Mac one TTL shorter (a UDP socket for probes, the ICMP socket for echo), and the errors macOS hands every unprivileged ICMP socket (checked: time exceeded for both echo and UDP probes, IP header included) turned into the errors the guest would have had, quoting what it sent, so its connection tracking passes them to the container. Full-TTL UDP reaching the card is still dropped: it means something escaped the streams.

**LAN mode's forward chain** accepts what answers a container's own packets (`ct state established,related`); a bridge container's ping to a LAN device left by `eth1` and its reply was dropped.

**localhost for host-network containers** (`guest/agent/src/loopback.rs`, `listeners.rs`). Init redirects a connection to `127.0.0.0/8` or `::1` to the agent, and so to the Mac's loopback, on a port in no guest listener's set (`local_tcp`, kept by the agent from the same scan as the host-network listeners, every port until its first look). Its SYN first waits in `nft queue 7` while the agent asks the VMM, over the DNS stream, `<port>.tcp.loopback.lighter.internal`; the VMM answers from a bind of that loopback address without `SO_REUSEADDR` (in use means something holds it: checked for listeners on the address, the wildcard and a dual-stack v6 socket), and a no is refused with a reset by a mark on the requeued SYN. Without that, the agent accepted every such connection and the Mac's refusal arrived as a reset after it, which `nc -z` reads as open. The bind holds the address for microseconds; a Mac service binding in that same instant would be refused once.

**Every DNS type where the Mac would ask** (`crates/lighter-vmm/src/sysdns.rs`). A question that is not A or AAAA goes, unchanged, to the nameserver the Mac's configuration picks for the name (`scutil --dns`, read when asked and kept ten seconds): the resolver whose domain is the longest the name is in (a VPN's, an `/etc/resolver` file's), else the first by order of those for every domain; its reply is exact, NXDOMAIN included. For `.local`, `DNSServiceQueryRecord`, which asks by multicast, and lingers 150 ms past its first answer for the rest; it says "no such record" at once while its cache is cold, so for these a negative is only what is left after a second and a half. That API also reports a missing name and a missing type alike (-65554 for both, checked), which is why it is not the path for the rest.

**UDP for host-network containers.** TPROXY sees only forwarded packets, so the guest's own UDP for beyond the card (which host-network containers share) left by `eth0` as packets and the card dropped them: `dig @8.8.8.8` from one timed out on 0.12.5. The output route chain now marks it, the policy route loops it through `lo`, and a divert rule for `lo` with that mark hands it to the agent (`fib daddr oif` reads `lo` for a looped packet, so the ordinary rule does not take it). Short-TTL probes and DHCP stay packets. UDP to loopback on a port nothing in the guest has bound (`local_udp`) is the Mac's loopback's, as TCP is; its first datagram waits in the same queue, kept in the guest when a socket there holds the port now, refused with port unreachable when the Mac has nothing.

**A connection to a guest listener too new for `local_tcp`** (a server that connects to itself as it starts; m3-streams' own self-test) went to the Mac and was refused: the agent now binds the port on the guest's loopback first, and "in use" keeps the connection in the guest by a mark the redirect skips.

**IPv6 on the link.** vmnet takes the Mac's IPv6 address too (`vmnet_host_ipv6_address_key`; checked: `bridge100` gets it, with the /64 on-link, no root). The /64 is unique-local, its global ID the network identifier's first five bytes. Docker's default bridge is `P:0:1::/80` and a network made with IPv6 an /80 of `P::/65`; the Mac is `P:0:ffff::1` and the card `P:0:ffff::2`, at the top, because Docker's IPv6 allocator, unlike its IPv4 one, does not step around a subnet the guest routes: with the card at `P::2/80`, a new network was given `P::/80` and a container the card's address. The link card answers the Mac's neighbour solicitations for every address in the /64 but the Mac's own (and never one from `::`, which is duplicate address detection), as `proxy_arp` does for IPv4: Linux proxies IPv6 neighbours one listed address at a time. Names carry AAAA records, and the NSEC lists the families a name has. Networks without IPv6 stay as Docker makes them; IPv6 on every network by default is a separate decision.

**Names of a container's own choosing**: `lighter.domains` (any `.local` name, the ones the Mac asks the link about; `*.` for all under one), and any name under a container's own (`api.web.lighter.local`). A `*.` name is answered, never announced: multicast DNS has no wildcards.
