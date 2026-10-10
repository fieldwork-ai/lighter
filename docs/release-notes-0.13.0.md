# lighter 0.13.0

Your Mac can reach every container directly, at its own address (IPv4 and IPv6) and by name, with no port published and nothing to install, one setting away; a host-network container's `localhost` is your Mac's; and containers see your Mac's DNS for every kind of record:

```
lighter config --direct on
lighter restart
docker run -d --name web nginx
curl http://web.lighter.local/
```

## Direct access

- **Every container has an address the Mac can reach**, IPv4 and IPv6. `docker inspect` shows it, and anything on the Mac connects to it: a browser, a database client, `ssh`, any port, published or not. The container sees the Mac's own address on that network as the caller, not a proxy's. IPv6 comes with the default bridge and with any network you make with it (`docker network create --ipv6`, or `enable_ipv6: true` in Compose), as Docker's own rules have it.
- **And a name.** A container named `web` is `web.lighter.local`; a Compose service is `<service>.<project>.lighter.local` too (`db.shop.lighter.local`, every replica's address), and each replica keeps its own name (`shop-db-1.lighter.local`). A host-network container's name is the machine's own address on that network. Anything under a container's name is that container too (`api.web.lighter.local`), for a proxy serving several sites, and a label gives one names of your choosing: `lighter.domains=myapp.local,*.myapp.local`. Names resolve in a fraction of a second, in both families, appear as a container starts and go as it stops. Containers resolve them too.
- **Only your Mac.** The network the containers are on is between the Mac and lighter alone: other devices on your network can't route to it, and the names are not announced anywhere. Published ports work exactly as before.
- **No root, no helper.** lighter makes this network itself, with the VM Networking entitlement Apple granted for 0.12.5. A build of your own, which can't hold the entitlement, starts without it and `lighter status` says so.

```
lighter status
  direct     containers at their own addresses (10.211.0.0/16) and as NAME.lighter.local
```

## Good to know

- **Container addresses move to `10.211.x.x`**, the range of lighter's network, chosen when the machine first starts so that it doesn't overlap anything your Mac already routes (your network, a VPN); another `10.x` range if it would. If something on your Mac needs that range, pick another with `lighter config --direct-subnet 10.42.0.0/16` and `lighter restart`.
- **Networks you made before upgrading keep their old addresses** (`172.x`), so the Mac reaches their containers by published ports only, as before, until you make them again: `docker compose down` then `docker compose up` for a Compose project. `lighter status` and `lighter doctor` list any such networks. The same applies to a network you create with a subnet of your own.
- **macOS may ask whether an app can find devices on your local network** the first time it connects to a container directly, as it does for a printer or a NAS. Apple's own tools, such as `curl`, are exempt.
- **Why it is a setting.** macOS turns on its packet filter for the whole Mac while a network like this one is up, for every app and not just lighter: on an M5 Ultra, loopback traffic between any two programs fell by a third (131 to 88 Gb/s), and a container's published port by 15 to 20% (still ahead of OrbStack, which turns it on too). Most work never notices, but lighter doesn't make that choice for you. `lighter config --direct off` and `lighter restart` give it back.
- Through the link a container answers at about 6 Gb/s; a published port stays the fast way for bulk transfers.

## What else changed

- **`localhost` in a host-network container reaches your Mac.** A container with `network_mode: host` that connects to `localhost:5432` reaches the database on your Mac, as it would if it ran on the Mac itself, over IPv4 and IPv6, TCP and UDP (a StatsD or syslog collector on the Mac, say). A port something in lighter listens on (another host-network container, a published port) still goes there first, as on Linux, and a port nobody listens on, anywhere, is refused straight away, so `nc -z` and wait-for-port checks keep telling the truth. Containers on Docker's own networks keep a `localhost` of their own, as always; they reach the Mac at `host.docker.internal`.
- **On a NAS, a container's permissions are the ones it set** (#69). A share's server decides permissions for itself: a QNAP refuses every `chmod`, even from the file's owner, and gives every new file `rwxrwxr-x`, and some servers accept a `chmod` and change nothing. So a container's `chmod` failed there, and a file it made private read back as `775`: Borg Backup Server stopped at its first `chmod`, Postgres refused a data directory that wasn't `0700`, and `ssh` a key that wasn't `0600`. lighter now keeps what a container sets on a network share where it keeps the owners a container sets, and where Docker Desktop keeps both, and still sets it on the file itself, so a server that accepts it shows your Mac and your network the same. A `chown` of a dot-file on such a share, which failed now and then, works too.
- **On a NAS, a file that replaces another reads as itself.** A share whose server offers no file ids (Samba's default with Apple's extensions, which many NAS turn on for Time Machine) has macOS number every file by its name, so a file renamed over another, or made again under its name, took over the numbers of the file it replaced, and lighter could serve the old file, or none, in its place. Postgres replaces its catalog cache that way and failed on its next connection with "cache lookup failed for index 2662"; a file written and renamed into place, as editors, package managers and databases save, read back empty now and then, and its rename could land before the file had been made, leaving it under its old name. lighter now tells such files apart by when they were made, and keeps a rename behind the file it moves; A file deleted while something still has it open, which a share keeps as `.smbdelete…` until it is closed, is no longer listed, as on Linux: Postgres's `initdb` tried to sync one and failed. Postgres runs with its data on such a share, its tables and indexes intact across restarts.
- **A NAS that stops answering stops only what touches it.** Two requests about a share were answered on the guest CPU that asked, and waited there for the NAS: looking up the folder a share is mounted on (a container starting with the share bind-mounted, or a listing of the folder holding it, `/Volumes` or your home), and the last close of a file on it. A NAS that hung stopped that CPU, and every request to the same shared folder from the others queued behind it. With the server in a container on the same machine, sharing back to the Mac, the stopped CPU was one the server needed, and the whole machine waited ten minutes for macOS to give up on it. Both now wait on a thread of lighter's.
- **Every DNS record type comes from your Mac's resolver.** Addresses always did, so a VPN's internal names, `/etc/resolver` files and `.local` names already worked in containers. TXT, SRV, MX, PTR and the rest went straight to your network's nameserver, which knows none of those: now each goes to the nameserver your Mac would ask for that name, with its exact answer, or to multicast DNS for `.local`. A VPN's internal SRV records resolve, and a container can browse the services on your network by DNS-SD (`dig PTR _services._dns-sd._udp.local`).

## Also

- **traceroute works from a container**, by UDP (`traceroute`, the default) and by ICMP (`traceroute -I`, `mtr`): the same route the Mac takes, after two hops inside, the container's bridge and lighter's gateway. Containers' traffic leaves through the Mac as streams, where nothing on the way could see a TTL run out, so traceroute found no hop at all. Probes with a short TTL now go as packets, sent on from the Mac one TTL shorter, and the routers' answers come back to the container, without root.
- **A host-network container's UDP reaches the internet.** Its own resolver (`dig @1.1.1.1`), NTP, QUIC and the like went nowhere: a container on Docker's network was carried, but the machine's own traffic, which host-network containers share, was not. It is now.
- **Containers resolve `.local` names on every Mac.** On some (an M1 on macOS 26.6, here), a container could resolve no `.local` name at all, a printer's or another Mac's, though the Mac itself could: macOS was telling lighter's machine nothing. lighter now also asks the network itself, and gets the answer the Mac would.
- **With LAN mode on, a container on Docker's network pings devices on your network** again. Its echo went out by the LAN card and the reply was turned away there.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script. Containers with a restart policy come back by themselves; start the others again, as after any restart. Direct access is off until you turn it on; when you do, containers move to their new addresses at the next restart.
