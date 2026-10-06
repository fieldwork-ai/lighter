# lighter 0.12.0

`network_mode: host` works from the Mac, and a container can be on your network: Home Assistant, in host mode, discovers your devices and is discovered by them, as it would be on a Linux machine (#60).

## What went wrong

lighter connects containers to your network through the Mac rather than putting them on it. That keeps your VPN and proxy settings in force for every container, and it meant two things never worked:

- **`network_mode: host`**: a host-network container shares lighter's own network, not the Mac's, and Docker has no port bindings to report for it, so nothing it listened on was reachable from the Mac.
- **Discovery**: Home Assistant, Plex, Jellyfin and the like find devices with mDNS, SSDP and DHCP, which only reach devices on the same network, so they found nothing on their own, in either mode.

## What changed

- **What a host-network container listens on is reachable from the Mac**, TCP and UDP, on `localhost` and, as `lighter config --publish` says, on the Mac's network, exactly as a port you publish with `-p` is: within milliseconds of the server starting to listen, gone when it stops, and the container sees who is calling. A server bound to `127.0.0.1` is on the Mac's loopback only.
- **LAN mode puts the machine on your network**, with a network card and an address of its own:

  ```
  sudo lighter lan enable      # once: installs lighter's network helper
  lighter config --lan on
  lighter restart
  lighter status               #   lan        192.168.50.85/24 on Wi-Fi en1
  ```

  A host-network container then discovers and is discovered as on a Linux machine: Home Assistant picks the LAN address as its own and advertises it, and its integrations find Hue bridges, Chromecasts, printers, HomeKit and Matter devices, and anything else that announces itself. The machine gets its address from your router; on Wi-Fi, where a router may lease it nothing (the Mac's Wi-Fi carries one MAC), name one with `lighter config --lan-address 192.168.50.240`.

  Only what needs your network uses it. Everything bound for the internet still leaves through the Mac, so your VPN and proxy settings still apply, and containers on Docker's own networks reach your devices as they always have. Your network can reach only the ports host-network containers listen on, and the ports you publish when `--publish` is `lan`: never lighter's own.

  LAN mode needs a small system helper, because macOS lets only the system put a virtual machine on a network. It serves only lighter, and only the users who enabled it, and `sudo lighter lan disable` removes it. lighter has asked Apple for the entitlement that will make the helper unnecessary.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script. Containers with a restart policy come back by themselves; start the others again, as after any restart. LAN mode is off until you turn it on.

## Also

- **A container that uses the GPU or Neural Engine as soon as it starts is no longer turned away.** Docker starts a container's process a moment before it lists the container as running, and lighter checks that list before letting a container reach an accelerator: a client that connected at once, as llama-bench does, could be refused as not running. lighter now waits out that moment, without holding up any other connection, and still refuses a container that did not ask for the device.
- **The machine gives memory back to a Mac that swapped long ago.** A Mac with a lot of swap in use was treated as short of memory for as long as the swap stayed, which can be days after whatever caused it, so the machine held on to memory it had finished with. Swap now counts only while the Mac is still swapping.
- Linux remains **6.18.52**, with one more option built in, UDP socket diagnostics, which is how lighter finds a host-network UDP service (kernel f4dfc278). The data epoch remains **1**.
- A machine has room for more devices: 32 instead of 16, so LAN mode and several shared folders of your own fit together.
