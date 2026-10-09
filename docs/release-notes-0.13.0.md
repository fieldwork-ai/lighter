# lighter 0.13.0

Your Mac now reaches every container directly, at its own address and by name, with no port published and nothing to install:

```
docker run -d --name web nginx
curl http://web.lighter.local/
```

## What changed

- **Every container has an address the Mac can reach.** `docker inspect` shows it, and anything on the Mac connects to it: a browser, a database client, `ssh`, any port, published or not. The container sees the Mac's own address on that network as the caller, not a proxy's.
- **And a name.** A container named `web` is `web.lighter.local`; a Compose service is `<service>.<project>.lighter.local` too (`db.shop.lighter.local`, every replica's address), and each replica keeps its own name (`shop-db-1.lighter.local`). A host-network container's name is the machine's own address on that network. Names resolve in a fraction of a second, appear as a container starts and go as it stops. Containers resolve them too.
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
- `lighter config --direct off` turns it off.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script. Containers with a restart policy come back by themselves, on their new addresses; start the others again, as after any restart.
