# Networking head to head: lighter, OrbStack, Docker Desktop, Colima

9 October 2026. One harness, the same checks against each runtime, on the same Mac and network, one runtime at a time.

Since: 0.13.0 closes three of the four gaps below: the Mac reaches containers by IP and by name ([design](design-0.13-direct-access.md)), traceroute answers the same hops as from the Mac, and ping to a device on the network works with LAN mode on.

## Method

- **The Mac under test:** the M5 Ultra Mac Studio (`192.168.50.25` on Wi-Fi, `en1`), with native IPv6 from the router and Tailscale connected. The daily lighter kept running throughout; every test port is in the 40000s.
- **Another device on the network:** the M1 (`192.168.50.21`, Wi-Fi), driven over ssh, for everything that has to come from outside the Mac: reaching published ports, seeing callers' addresses, discovery, broadcasts, throughput.
- **Runtimes:**
  - lighter 0.12.5: a private machine from the release build, LAN mode on, bridging Wi-Fi by itself (`192.168.50.65` by DHCP).
  - OrbStack 2.2.3: engine 29.4.0, defaults (its "network bridge" on, its privileged helper installed).
  - Docker Desktop 4.94.0: engine 29.8.2, defaults. Host networking is off by default; turning it on needs a signed-in Docker account, so it is measured off.
  - Colima 0.10.3: `--network-address --network-mode bridged --network-interface en1`, which runs its vmnet helper with `sudo`. It got `192.168.50.138` by DHCP.
- **The harness** is `.context/h2h/h2h.py` (one Python file; results as JSON per runtime). Requests from the Mac go through `/usr/bin/curl`, because macOS's Local Network privacy refuses an unapproved Python's connections to local-network addresses with `EHOSTUNREACH`, which first read as two runtimes' failures.

## Results

| Check | lighter | OrbStack | Docker Desktop | Colima |
|---|---|---|---|---|
| **Published ports** | | | | |
| TCP from the Mac, the Mac's LAN address, another device | yes | yes | yes | yes |
| UDP from the Mac and another device | yes | yes | yes | no |
| IPv6 (`[::1]`, the Mac's global address from another device) | yes | yes | yes | no |
| Bound to one Mac address only | yes | yes | yes | no (refused) |
| A 100-port range | yes | yes | yes | yes |
| First answer after `docker run` | 0.27 s | 0.34 s | 0.44 s | 0.48 s |
| **The caller's own address reaches the container** | | | | |
| TCP from another device | yes | no (`192.168.215.1`) | no (`192.168.65.1`) | no (`172.17.0.1`) |
| UDP from another device | yes | no | no | no |
| **Host networking** | | | | |
| Reachable from the Mac and from other devices | yes | yes | no (off by default) | yes |
| Its own address on the network | yes | no | no | yes (with `sudo`) |
| The container's `localhost` is the Mac's | no | yes | no | no |
| **Discovery** (host-network container) | | | | |
| Hosts answering an mDNS query | 12 | 1 (the Mac, on OrbStack's own network) | 0 | 0 |
| Hosts answering an SSDP search | 11 | 0 | 0 | 0 |
| Its mDNS announcement seen by another device | yes | no | no | no |
| Its broadcast reaches another device | yes (from its own address) | no | yes (from the Mac's) | yes |
| **Outbound** | | | | |
| HTTPS, DNS over UDP, `.local` names, Tailscale names and peers | yes | yes | yes | yes |
| IPv6 to the internet | yes | no (off by default³) | no | no |
| ping to the internet | yes | yes | yes | yes¹ |
| ping to a device on the network | no² | no | yes | yes |
| traceroute hops answered (of 6) | 0 | 5 | 1 | 1 |
| **The Mac reaching a container directly** | | | | |
| By the container's IP | no | yes | no | no |
| By name (`name.orb.local`) | no | yes | no | no |
| **Speed** | | | | |
| Mac to container, one TCP stream | **78 Gb/s** | 50 | 12 | 3 |
| Container to Mac | **106 Gb/s** | 80 | 30 | 2.7 |
| Mac to container, four streams | **85 Gb/s** | 40 | 19 | 2.8 |
| Container to container | **177 Gb/s** | 101 | 92 | 101 |
| Round trip, Mac to container, p50 | **44 µs** | 55 | 92 | 187 |
| Connections per second (connect, one exchange, close) | 6550 | 6612 | 3026 | 1697 |
| UDP at a 2 Gb/s offer | 2.0, 0.15% lost | 2.0, 0.03% | 1.98, 0.9% | — |
| Another device to a container, over Wi-Fi | 0.14 | 0.14 | 0.12 | 0.13 |
| Download from the internet (native 182–206 Mb/s) | 200 | 170 | 189 | 171 |

¹ Colima's pings to the internet and to a Tailscale peer came back in 0.2–0.3 ms, faster than the network allows: its user-mode network stack answers them itself.

² Only with LAN mode on: with LAN mode off, lighter's containers ping devices on the network (checked on the daily machine: the M1 and the router answer).

³ OrbStack's docs: "For containers, IPv6 is disabled by default for compatibility"; it is one setting ("Enable IPv6"). Measured as installed.

## What it says

**Where lighter leads:**
- **On the network.** It is the only runtime whose containers discover devices (12 mDNS and 11 SSDP responders), can be discovered, and get an address of their own without `sudo`. Colima bridges too, with `sudo`, but its host-network containers discovered nothing (why was not checked; its default route is its other card).
- **Callers' addresses.** The only one where a container sees who is calling (Pi-hole, logs, rate limits, allow lists).
- **IPv6 out by default.** The only one whose containers reached the internet over IPv6 as installed; OrbStack has it behind a setting.
- **Speed between the Mac and containers:** 1.6 times OrbStack from the Mac to a container and 1.3 times back; 6 and 3.5 times Docker Desktop; 25 and 39 times Colima; and the lowest round trip.

**Where lighter is behind:**
1. **The Mac cannot reach a container directly**, by IP or by name. OrbStack does both (`name.orb.local`), without admin rights: its FAQ says admin is optional for every feature, the container range is routed to a network interface it creates on the Mac, and `.orb.local` is resolved like any `.local` name.
2. **A host-network container's `localhost` is not the Mac's.** OrbStack shares it both ways, so a container reaches a Mac service on `localhost` with no `host.docker.internal`.
3. **traceroute shows nothing** (0 hops of 6; OrbStack 5).
4. **With LAN mode on, ping to devices on the network is lost** (a bug, not a design limit).

**Even:** ports bound to one address, port ranges, reaching the Mac from containers, DNS (including `.local` and VPN names), VPN reachability, connection rate.

## Not measured here

- Changes of network: switching Wi-Fi, Wi-Fi to Ethernet, a VPN connecting and disconnecting, sleep and wake, the Mac's address changing.
- LAN mode over Ethernet and USB or Thunderbolt adapters (#60's reporter, on `en8`, discovers nothing so far).
- A server listening on IPv6 only, and inbound IPv6 callers' addresses.
- Per-domain resolvers from `/etc/resolver` (split DNS) inside containers.
- Corporate VPNs and HTTP proxies.
- Apple's `container`, which is not Docker-compatible.
