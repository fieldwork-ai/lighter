# lighter 0.12.3

A device that calls a published UDP port over an IPv6 link-local address (`fe80::`) now gets an answer.

## What went wrong

lighter passes each caller's own address to a published port, so a DNS server such as Pi-hole or Technitium sees which device is asking. A link-local IPv6 address cannot be passed on: it only means something on the Mac's own network, and a container's reply to it would never leave the container. Over TCP such a caller was shown as lighter's own address, as a caller on the Mac itself is. Over UDP it was not shown at all: the datagram was dropped, and the caller got no answer.

## What changed

- **A link-local IPv6 caller is answered over UDP as well as TCP,** and shown as lighter's own address, as a caller on the Mac itself is. Callers on IPv4 and on global IPv6 addresses are shown as themselves, as before.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- Linux remains **6.18.52** (kernel f4dfc278). The data epoch remains **1**.
