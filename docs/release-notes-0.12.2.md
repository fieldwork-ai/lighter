# lighter 0.12.2

A port published on one of your Mac's own addresses (`-p 192.168.1.20:9000:9000`, or a Tailscale address) now works, as it does with Docker Desktop and OrbStack.

## What went wrong

Docker runs inside lighter's Linux machine, and binds a published port on the address you name. Name one of the Mac's addresses rather than `0.0.0.0` or `127.0.0.1`, and Docker could not bind it, because the machine does not have that address: the container did not start ("cannot assign requested address"). A Compose file that keeps MinIO, a database or an admin UI on one network, or only on Tailscale, could not be used.

## What changed

- **A port published on a Mac address is served on exactly that address.** lighter binds it on the Mac, and the container answers there and nowhere else: not on `localhost`, and not on the Mac's other addresses. TCP and UDP, IPv4 and IPv6.
- **Containers still reach the Mac on that address.** A container talking to a service running on the Mac itself, at the same address, still reaches the Mac.
- If the address is not there when the port is opened (Tailscale disconnected, Wi-Fi on another network), `lighter status` lists the port as not forwarded, with the reason, and lighter keeps trying, as for any port the Mac cannot open.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- Linux remains **6.18.52** (kernel f4dfc278). The data epoch remains **1**.
