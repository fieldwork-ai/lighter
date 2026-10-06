# lighter 0.12: host networking and the LAN

Status: design, 2026-10-06. Issue #60: Home Assistant in `network_mode: host` is unreachable from the Mac, and in any mode it discovers nothing on the user's network.

## Goals

1. **`network_mode: host` works from the Mac.** What a host-network container listens on is reachable on the Mac's `localhost`, and on its network when the publish scope says so (`lighter config --publish`), exactly as a port Docker publishes is.
2. **A container can be on the user's network.** As an option, the machine joins the Mac's network as a device of its own, so a host-network Home Assistant discovers devices (mDNS, SSDP, DHCP sniffing, Matter's IPv6), is discovered by them, and is reachable at an address of its own, as it would be on a Linux machine.
3. **Nothing changes for anyone who does not ask.** No new privileges, no new idle cost, no new exposure of the guest by default.

Non-goals for 0.12: a discovery relay that works without joining the network (below, *Alternatives*); giving individual bridge-network containers addresses on the LAN (`macvlan`/`ipvlan` over the LAN card would follow from goal 2, and is left to users until asked for).

## What was measured first

On this Studio (macOS 27.0.1, M5 Ultra), whose network is Wi-Fi (`en1`; its Ethernet `en0` is unplugged), as are the M5 Pro's and the M1's. Wi-Fi is the common case, not an edge.

- **Today**: a host-network server on lighter, Docker Desktop and OrbStack: reachable from the Mac only on OrbStack. Discovery from a container (an mDNS service query, an SSDP search, five seconds of listening): nothing on lighter or Docker Desktop in either mode, one answer on OrbStack in host mode, against 29 service types the Mac's own resolver sees on this network.
- **The guest's listeners carry their owner** (`ss --cgroup` in the guest): a host-network container's socket is `cgroup:/docker/<id>`; Docker's own proxies and the engine are `cgroup:/engine`. So "a listener in the guest's root network namespace whose cgroup is under `/docker/`" is exactly "a host-network container's listener", with nothing of lighter's or Docker's in it, and no list to keep.
- **vmnet bridged mode over Wi-Fi** (a 250-line C experiment as root, `.context/bridge-spike/spike.c`): the interface comes up (MTU 1500, frames up to 1514 bytes). Received: the LAN's broadcast and multicast, mDNS from four hosts in five seconds. With a static address of its own (`192.168.50.241`): the router's ARP reply, 3/3 pings to the router, an mDNS query answered by 9 hosts, 5/5 pings from the M5 Pro across the LAN, 3/3 from the Studio itself.
- **DHCP over Wi-Fi does not give the guest an address.** Wi-Fi carries one MAC per station, so macOS bridges Wi-Fi with MAC address translation: the guest's DISCOVER leaves with the Mac's MAC as source *and* as `chaddr`, the broadcast flag cleared. The router (ASUS, dnsmasq) keys the lease on `chaddr`, ignores the client identifier the guest sends (option 61, which survives), and offers the Mac's own address. On Ethernet there is no translation and DHCP is ordinary; that was not testable here (no cable), and is how every bridged VM on Ethernet works.
- **Home Assistant chooses its "default" adapter by the source address of a UDP socket connected to `224.0.0.251`** (`homeassistant/components/network/util.py`, `async_get_source_ip(MDNS_TARGET_IP)`), and advertises (zeroconf, HomeKit, SSDP) on that adapter.
- **vmnet's bridged mode needs root or `com.apple.vm.networking`.** The macOS 26 network API (`vmnet_network_create`, with XPC serialization to hand a network to another process) offers shared and host-only modes only. So without the entitlement, frames for a bridged interface must be relayed by a root process. The entitlement was requested from Apple on 2026-10-06.

## Part 1: host-network ports, forwarded from the Mac

```
 Mac                                   guest
 ┌──────────────────────────┐         ┌───────────────────────────────────────┐
 │ PortWatcher              │ docker  │ dockerd                               │
 │  Docker's published      │◄──API───┤                                       │
 │  ports  ∪  host-network  │         │ agent (control): `listeners`          │
 │  listeners               │◄─vsock──┤  sock_diag: LISTEN + bound UDP in the │
 │        │                 │         │  root netns, cgroup under /docker/    │
 │        ▼ all or nothing  │         │                                       │
 │ PortMapper: bind :8123   │ stream  │ agent (inbound): connect as the       │
 │  accept ─────────────────┼────────►│  client to 192.168.127.2:8123 ──► HA  │
 └──────────────────────────┘         └───────────────────────────────────────┘
```

**Finding the listeners.** The agent's control process answers a new verb, `listeners`, with one line: every TCP socket in `LISTEN` and every bound, unconnected UDP socket outside the ephemeral port range, in the guest's root network namespace, whose cgroup is a container's (`/sys/fs/cgroup/docker/<id>` or below it). It asks the kernel once per call over `NETLINK_SOCK_DIAG` (`inet_diag` dumps for v4 and v6, TCP and UDP), which reports each socket's cgroup id (`INET_DIAG_CGROUP_ID`) and, for v6, whether it is v6-only; cgroup ids are matched to containers by the inode of each container's cgroup directory. No `/proc` walk, no dockerd.

- A v6 wildcard listener that is not v6-only (Linux's default) is reported on both families, since it answers `127.0.0.1` too.
- UDP sockets in the ephemeral range are a container's own lookups, not services, and would make forwards flap.
- A bind to `127.0.0.1` or `::1` is forwarded to the Mac's loopback only; a bind to the guest's own address counts as the wildcard; a bind to a Docker bridge address or to the LAN card's address (Part 2) is not forwarded.

**When to ask.** The Mac's port watcher already reads `/containers/json` on every container event; it now also notes which running containers use the host network. While one does, it asks the agent for listeners every second, and on every event; while none does, it never asks. That costs nothing to anyone without a host-network container, and one round trip a second, against a container that is busy by nature, to anyone with one. A BPF doorbell (a `sock_ops` program on `TCP_LISTEN_CB`, `post_bind` for UDP) could replace the second's poll; it is not worth four more hand-assembled programs while the poll is free in practice, and the design leaves room for it: the doorbell would only prompt the same `listeners` call.

**Forwarding.** A host-network listener becomes the same `Published { proto, addr, port }` a Docker publish is, so everything 0.11.5 built applies unchanged: all-or-nothing per port across families, retry on its own, `lighter status`/`doctor` naming a port the Mac refuses. A listener that disappears is withdrawn at the next answer. A port both published by Docker and listened on by a host-network container is one forward.

**The reply path (the one guest change).** The agent dials a published port as the client (`IP_TRANSPARENT`, mark `0x4c494e42`), and the container's replies come back through `prerouting`, where the mark routes them to the agent's socket. A host-network listener's replies are local output, which no rule matched, so they left by `eth0` and the dial timed out before falling back to dialling as the guest. One rule in the `type route hook output` chain gives local replies of those connections the reply mark, and the existing policy route delivers them:

```
ct direction reply ct mark 0x4c494e42 meta mark set 0x4c494e43
```

The container then sees its real client, as a bridge-network container does (Pi-hole's per-client view, Home Assistant's `trusted_proxies`).

## Part 2: the machine on the user's network ("LAN mode")

```
 Mac                                                     guest
 ┌──────────────────────────────────────┐               ┌────────────────────────────┐
 │ lighter-bridge (root, launchd)       │               │ eth0  192.168.127.2        │
 │   vmnet_start_interface(BRIDGED, en1)│               │   default route: streams   │
 │        ▲ frames                      │               │   through the Mac          │
 │        ▼                             │  virtio-net   │                            │
 │   socketpair(AF_UNIX, SOCK_DGRAM) ───┼── #2 (LanBackend) ── eth1  192.168.50.241   │
 │        ▲ fd passed on connect        │               │   LAN subnet on-link       │
 │ lighter (VMM) ───────────────────────┘               │   224.0.0.0/4, ff00::/8    │
 └──────────────────────────────────────┘               │   firewall: host ports only│
                                                        └────────────────────────────┘
```

**A second network card, not a replacement.** `eth0` and the streams stay exactly as they are: the default route, DNS, published ports, VPN and proxy settings honored because every connection is the Mac's. `eth1` is bridged to the Mac's network card and carries only what belongs on the LAN:

- the LAN's own subnet, on-link, so a container talks to a device directly and the device's replies come straight back;
- multicast, `224.0.0.0/4` and `ff00::/8`, so discovery uses the LAN, and so Home Assistant's choice of default adapter (the source address towards `224.0.0.251`) lands on `eth1` and it advertises an address devices can reach;
- IPv6 router advertisements on `eth1` for the LAN's prefixes and link-local addresses (`accept_ra=2`, since the guest forwards), without their default route (`accept_ra_defrtr=0`): Matter and other IPv6-on-the-LAN protocols work, the guest's IPv6 internet stays on the streams.

The redirect rules match `eth0` only, so nothing on `eth1` enters the streams; this falls out of the existing ruleset.

**Getting frames: the helper now, the entitlement later.** Both produce the same thing for the VMM, a source of Ethernet frames behind `NetBackend`:

- *With the entitlement*, the VMM calls `vmnet_start_interface` itself.
- *Without it*, `lighter-bridge`, a root daemon, does, and relays frames over a `SOCK_DGRAM` socket pair whose other end it passes to the VMM (`SCM_RIGHTS`) after a handshake on its control socket: one datagram per frame, so frame boundaries come free, and a full socket drops a frame as a full NIC ring does. The control connection is the interface's lifetime: the VMM exiting, or crashing, stops the interface.

The helper is installed once, `sudo lighter lan enable`, as a launchd daemon (`/Library/LaunchDaemons/dev.lighter.bridge.plist`, the binary copied to `/Library/PrivilegedHelperTools/`), socket-activated so it runs only while a machine uses it. It does one thing:

- It serves only the user who enabled it (the uid recorded at install) and root, and only a client whose code signature is lighter's (`identifier "dev.lighter.machine"` and team `N7N6BNF95K`, checked from the connection's audit token with `SecCodeCreateWithAuditToken`), so another account, or another program, cannot borrow raw access to the network through it.
- It opens bridged interfaces only, one per connection, at most eight at once.
- Its protocol is versioned in the handshake; a lighter that needs a newer helper says so (`lighter doctor`: run `sudo lighter lan enable` again), so upgrades do not require root unless the protocol changes.
- `sudo lighter lan disable` removes it.

`SMAppService` (a daemon inside `lighter.app`, approved once in System Settings, updated with the app) is the better long-term home and replaces the sudo install when it is proven from lighter's per-home app bundle; the protocol and the VMM side are the same either way.

**The address.** The card has a MAC of its own, random and locally administered, kept in the machine's home so that a router's lease and any reservation survive restarts.

- *On Ethernet*, DHCP, as any device: client identifier, hostname `lighter`.
- *On Wi-Fi*, DHCP is attempted, and its answer is rejected if it is one of the Mac's own addresses (the measured case); a router that keys leases on the client identifier gives the guest one of its own. Otherwise the user names one: `lighter config --lan-address 192.168.50.240`, with the subnet, gateway and DNS taken from the Mac's interface. `lighter status` shows the address, and `doctor` says why one is needed when it is.
- The address is the guest's, not the agent's to invent: the agent runs DHCP on `eth1` (a small client of its own, so that it can decline the Mac's address and report what it got), or configures the static one, and announces it (gratuitous ARP, unsolicited NA).

**What the LAN can reach.** A machine on the network is also reachable from it, and the guest listens on more than a container's ports. An `inet lighter-lan` table filters `eth1`:

- *input*: established and related, ICMP and ICMPv6 (neighbour discovery), DHCP replies, multicast; new connections only to the ports host-network containers listen on, kept in an nft set by the agent from the same `listeners` scan as Part 1; everything else dropped (the agent's ports, Docker's proxies);
- *forward*: Docker's published ports, which Docker's DNAT also applies to `eth1`, pass when the publish scope is `lan` and are dropped when it is `localhost`, so `--publish localhost` keeps meaning "not on the network".

**Which network card.** `auto` (the default) is the Mac's primary interface when the machine starts, if vmnet can bridge it (`vmnet_copy_shared_interface_list`); `lighter config --lan-interface en0` pins one. A change of primary interface while running is reported by `doctor` and takes effect at the next restart; moving the bridge live (link down, re-bridge, link up, DHCP again) is follow-up work.

**Turning it on.**

```
lighter config --lan on        # the machine joins the Mac's network at the next restart
sudo lighter lan enable        # once, unless lighter has Apple's entitlement
lighter restart
lighter status                 #   lan        192.168.50.241 on Wi-Fi (en1)
```

## Alternatives considered

- **A discovery relay with no bridge**: the Mac joins mDNS and SSDP on its own sockets and relays into the guest, rewriting advertised addresses to the Mac's. It needs no root and no address, and Wi-Fi does not matter, but it covers two protocols (not DHCP sniffing, broadcast-based discovery such as LIFX, Kasa or Tuya, or Matter's IPv6), and rewriting what a container advertises is a translation layer forever chasing protocols. If the entitlement is refused and the helper's install proves a barrier, it is the next step for people who will not join the network; it shares nothing with Part 2 that would be wasted.
- **Making `eth1` the default route**: the guest would be an ordinary LAN host, but every connection would bypass the Mac's VPN and proxy settings, and the guest's internet would depend on a Wi-Fi bridge's translation table. The multicast route gets Home Assistant's choice right without it.
- **A root helper for everything (Lima's socket_vmnet), or sudo per start**: the helper is opt-in, relays one interface, and is not in the path of anything else.
- **`macvlan` per container**: on Wi-Fi each container's MAC is translated away, and a separate address per container needs a router that hands them out; not the default, available to users over `eth1`.

## Testing

- **Unit**: the listener rules (families, v6-only, ephemeral UDP, bind addresses, cgroup attribution) over captured `sock_diag` replies; the helper's handshake and its refusals (wrong uid, wrong signature, wrong version); the DHCP client's state machine, declining the Mac's address.
- **m3-publish**: a host-network TCP server and UDP echo reachable on the Mac's `localhost` and at the Mac's own address, the container seeing the Mac as its client, the forward withdrawn when the server stops and when the container exits, a `127.0.0.1` listener reachable on the Mac's loopback only.
- **m17-lan** (needs the helper; skips naming why when it is not installed): the guest gets a LAN address (static on Wi-Fi, chosen free by ARP probe); a host-network container hears mDNS from the LAN and is answered; the source address towards `224.0.0.251` is the LAN address; the Mac reaches a host-network server at the guest's LAN address; the agent's ports and a localhost-scoped published port are not reachable there; stopping the machine stops the interface.
- **Performance**: m5-speed unchanged; LAN throughput between a LAN host and a host-network container through the helper, recorded.

## Plan

1. Part 1: the `listeners` verb and its tests; the port watcher's host-network set; the reply rule; m3-publish checks.
2. Part 2, guest: `eth1` bring-up, DHCP client and static address, routes, `lighter-lan` firewall and its port set.
3. Part 2, Mac: `LanBackend`, persistent MAC, `lighter-bridge` with its install/enable/disable, the entitlement path behind the same interface; `config`, `status`, `doctor`.
4. m17-lan; qualification on the Studio and the M1; release notes; 0.12.0.
