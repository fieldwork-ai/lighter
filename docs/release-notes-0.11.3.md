# lighter 0.11.3

Five fixes reported by people running lighter every day: a request to a published port no longer hangs on a machine that has been up for a while, file watchers in containers see edits made on the Mac, a non-root container no longer slowly exhausts lighter's open files, `host-gateway` reaches the Mac, and rootless containers can use TUN.

## What changed

- **Requests to published ports no longer hang after days of uptime.** On a busy machine that had been up for a while, about one connection in 130 from the Mac to a container's published port lost its reply: the client waited with nothing back, or got an empty reply, and a client without a timeout waited forever. lighter joins each such connection to the container inside Linux, and finds a connection's other end in a table that, once full, made room by forgetting entries, sometimes those of connections still in use. A connection kept open between requests, as HTTP clients do, was the first to be forgotten. The table now forgets a connection only when it closes, and it cannot fill. On 0.11.2, 50 connections left open while 40,000 others came and went all hung on their next request; on 0.11.3 all 50 answer.
- **File watchers see changes made on the Mac** ([#43](https://github.com/fieldwork-ai/lighter/issues/43)). A file you saved on the Mac in a bind-mounted folder was visible in the container at once, but nothing told the container's file watchers: Node's `fs.watch`, chokidar and so Vite's dev server never reloaded. A change on the Mac now raises the events a change inside the container would: a new file, a write and the close after it, a deletion, and a rename as a deletion and a creation. Changes made inside the container are reported once, as before.
- **A non-root container no longer exhausts lighter's open files** ([#46](https://github.com/fieldwork-ai/lighter/issues/46)). Every file created in a bind mount by a container running as a user other than root kept one file open in lighter until it stopped. A `pnpm install` repeated as `user: 1000:1000` took lighter to macOS's limit on open files in a few days, after which creates failed and `docker` commands answered intermittently. Those files are now closed as soon as the container is done with them. lighter also stops spinning a core if it ever does reach the limit.
- **`host-gateway` reaches the Mac** ([#44](https://github.com/fieldwork-ai/lighter/issues/44)). `--add-host name:host-gateway`, and Compose's `extra_hosts: ["name:host-gateway"]`, named an address inside lighter's machine where nothing listens. It now names the Mac, the same address `host.docker.internal` does, so one Compose file works on lighter, Docker Desktop and Colima alike.
- **Rootless containers can use TUN and FUSE** ([#45](https://github.com/fieldwork-ai/lighter/issues/45)). `/dev/net/tun` and `/dev/fuse` had the modes Linux gives them before anything adjusts them, readable by root alone, so a rootless Docker-in-Docker (`docker:dind-rootless`) could not set up its network. They now have the modes every Linux distribution gives them. Opening either grants nothing on its own: making an interface still takes the network capability, and mounting still takes a mount.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script. Containers with a restart policy come back by themselves; start the others again, as after any restart.

## Also

- Linux remains **6.18.52**, with one patch added for the file events (kernel patch 0047). The data epoch remains **1**.
