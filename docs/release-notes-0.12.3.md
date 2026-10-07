# lighter 0.12.3

A network drive on the Mac that stops answering no longer freezes lighter's machine, and a device that calls a published UDP port over an IPv6 link-local address (`fe80::`) now gets an answer.

## What went wrong

**A hung network drive froze part of the machine.** lighter answers a file request on the CPU that asked for it when nothing else is waiting, which is faster than handing it to a worker. A request on a network drive (SMB, NFS, AFP, WebDAV) that had stopped answering then stopped that CPU for as long as the drive did: with a NAS hung for 48 seconds, three of the machine's CPUs stopped, one for 40 seconds, and any container on them stopped too, whatever it was doing.

**A link-local IPv6 caller got no answer over UDP.** lighter passes each caller's own address to a published port, so a DNS server such as Pi-hole or Technitium sees which device is asking. A link-local IPv6 address cannot be passed on: it only means something on the Mac's own network, and a container's reply to it would never leave the container. Over TCP such a caller was shown as lighter's own address, as a caller on the Mac itself is. Over UDP it was not shown at all: the datagram was dropped, and the caller got no answer.

## What changed

- **Only requests on the Mac's own disks are answered on the CPU that asked.** A request on a network drive goes to a worker, so a drive that hangs holds up only what is waiting for it: with the same NAS hung for 48 seconds, no CPU stopped. Requests on the Mac's own disks are as fast as before.
- **A link-local IPv6 caller is answered over UDP as well as TCP,** and shown as lighter's own address, as a caller on the Mac itself is. Callers on IPv4 and on global IPv6 addresses are shown as themselves, as before.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- Linux remains **6.18.52** (kernel f4dfc278). The data epoch remains **1**.
