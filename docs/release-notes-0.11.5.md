# lighter 0.11.5

A port a container publishes that the Mac will not give lighter is no longer half open, no longer stays closed after the Mac lets go of it, and no longer fails without telling you.

## What went wrong

When a container publishes a port, lighter opens it on the Mac on IPv4 (`0.0.0.0`) and IPv6 (`[::]`) separately. macOS refuses lighter a port that another user's program already holds on one of the Mac's addresses, whatever lighter asks for: Tailscale Serve sharing port 9000 on the Mac's tailnet address is one example. When that happened:

- **One half opened anyway.** The container answered on `localhost`, which reaches IPv6, but not on `127.0.0.1`, and not to other containers through `host.docker.internal`, which is far harder to diagnose than a port that is simply closed.
- **Only a log file said so.** `docker run` and `compose up` succeeded, `docker ps` listed the port as published, and the container reported healthy.
- **Nothing tried again.** Once the other program let go, the port stayed closed until some unrelated container happened to start or stop.

## What changed

- **A port is opened on every address it was published on, or on none.** If the Mac refuses one, lighter closes the other rather than leave it half open.
- **lighter tries again by itself,** after a second at first and then less often, up to every 30 seconds, so the port opens shortly after whatever held it lets go.
- **It tells you.** `lighter status` lists each published port it could not open and why, and `lighter doctor` warns about them. The log records the failure once, not on every attempt.

The refusal itself is macOS's, and every Docker runtime for the Mac meets it. If something on your Mac needs the same port, give one of them a different port.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script. Containers with a restart policy come back by themselves; start the others again, as after any restart.

## Also

- Linux remains **6.18.52** (kernel 6225825f, unchanged). The data epoch remains **1**.
- Docker still reports success when it starts such a container. Failing the start, as Docker Desktop does, means lighter answering Docker's API itself, and is a change for another release.
