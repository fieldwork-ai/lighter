# lighter 0.12.5

LAN mode needs no `sudo` and no helper any more. Apple granted lighter its VM Networking entitlement, so lighter puts its machine on your network by itself.

## What changed

- **LAN mode is one setting.** `lighter config --lan on`, then `lighter restart`: the machine gets its own address on your network, with no `sudo lighter lan enable` first and no root process installed on your Mac. Until now a small helper, a root launchd daemon, started the bridged network card for lighter; macOS lets an app do that itself only with an entitlement Apple grants case by case, and Apple has granted it to lighter.
- **Everything else about LAN mode is the same**: the same address from your router, the same discovery of devices by mDNS and SSDP, and the same rules for what your network can reach. Measured on the release build with no helper anywhere: an address by DHCP on Wi-Fi, 15 hosts answering a host-network container's mDNS query, and every reachability check of the LAN gate passing.
- **If you installed the helper**, it can go: `sudo lighter lan disable`. Leaving it does no harm; lighter no longer uses it. `lighter lan status` says which applies.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- A build of your own from source still needs the helper for LAN mode (`sudo lighter lan enable`): the entitlement holds only in a release signed and notarized by Fieldwork.
- Linux remains **6.18.52** (kernel f4dfc278). The data epoch remains **1**.
