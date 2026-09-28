# lighter 0.10.2

Two changes to published ports: a container can publish port 53, so Pi-hole, AdGuard Home or any DNS server serves your network from lighter; and a container behind a published port now sees who is calling, the client's own address, where it used to see lighter's.

## A DNS server on port 53

On 0.10.1 `docker run -p 53:53/udp …` failed with "address already in use". The guest's own resolver, which answers containers' DNS through the Mac, held port 53 inside the guest, and Docker could not reserve it for a container. The resolver now listens on a private port, and the guest's firewall sends containers' queries to it, so they resolve exactly as before and port 53 is free to publish.

```yaml
services:
  pihole:
    image: pihole/pihole:latest
    ports:
      - "53:53/udp"
      - "53:53/tcp"
      - "8088:80/tcp"
    environment:
      TZ: Europe/London
      FTLCONF_webserver_api_password: change-me
      # Answer every client on the network, not only this container's own
      # subnet: devices now arrive with their own addresses (below).
      FTLCONF_dns_listeningMode: all
```

Point your router's DNS, or a device's, at the Mac's address. On a home network, another Mac on the LAN querying Pi-hole on an M5 got its answers in 7 ms, over UDP and TCP, with ad domains answered `0.0.0.0`. Other containers on the same machine keep resolving through the Mac, not through the published server.

## Published ports see the real client

A container behind a published port used to see every connection as coming from lighter (`192.168.127.2`), so Pi-hole showed one client, and a web server's log, a rate limiter or an allow list saw one caller. The Mac now passes each connection's client address to the guest, which connects to the container as that client, over TCP and UDP. A server sees the device on your network that called it. IPv6 clients take the same path; the release was checked on IPv4 networks, where the release Macs have no global IPv6 address.

- Connections from the Mac itself on loopback (`localhost`) arrive as before, from lighter's address: the guest cannot usefully claim to be the Mac's loopback.
- A server that only answers its own subnet by default, as dnsmasq and Pi-hole's "local" listening mode do, now sees network clients as foreign and ignores them. Set it to answer every origin (Pi-hole: `FTLCONF_dns_listeningMode: all`, or "Permit all origins" in its settings).

## How

The guest agent's resolver moved from `192.168.127.2:53` to `:15353`, with nat rules in the guest sending `192.168.127.2:53` to it; a flow the Mac opens to a published port carries a mark those rules skip. The published-port stream's header gained the client's address after the destination's, flagged in its family byte, and the agent binds its socket to the client transparently. The container's replies, addressed to the client, are marked by the connection and routed back to the agent by a policy rule of their own, rather than out as outbound traffic (`guest/rootfs/init`, `docs/architecture.md`).

Gate m3-publish publishes a DNS server on port 53, queries it over UDP and TCP on loopback and the LAN address, and checks that a published server sees a LAN client as itself and loopback as the guest; on 0.10.1 those checks fail. `scripts/test-inbound-rules.py` holds the guest's rules to the agent's constants.

## Also

- Linux remains **6.18.52** (kernel 9396cca3); the data epoch remains **1**.
