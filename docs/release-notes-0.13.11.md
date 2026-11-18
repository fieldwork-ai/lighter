# lighter 0.13.11

A port below 1024 published on one address, `127.0.0.1:80` for a reverse proxy moved over from Docker Desktop, now opens, through a small root helper you install once (#113). And a guest that is never quite idle now gives the Mac back what it frees within a minute, where one stack of agent sandboxes kept 9.6 GiB on the Mac for a day and a half (#112). Thanks to @k0d3r1s and @petrfilip for reports that came with the cause already found.

## What went wrong

**A port below 1024 published on one address was never forwarded** (#113). macOS lets a process that is not root bind a port below 1024 only on every address at once; on one address, loopback included, it needs root. So `-p 127.0.0.1:80:80` was refused, retried forever, and `lighter doctor` said another process held the port, which nothing did. The same went for any port below 1024 under `--publish localhost`, which keeps every publish on loopback: that mode could not publish 80 or 443 at all.

**A steady container load kept memory the guest had freed** (#112). The guest returns free memory to the Mac in runs of 32 KiB once its containers are at rest, and only in runs of 2 MiB while they work, which keeps an install from handing back memory it is about to take again. A load that is never at rest for eight seconds, such as builds, health checks and language servers with a burst every second or two, never went back to the small runs, and memory freed in pieces under 2 MiB stayed on the Mac for as long as the load lasted.

## What changed

- **lighter's helper binds ports below 1024 on one address.** `sudo lighter helper install`, once, installs it as a launchd daemon that runs only while asked. It binds the port for lighter, after checking that the caller is Fieldwork's lighter run by a user who installed it, and hands the socket over. It binds only what macOS keeps for root, and never under a port another program already listens on. A running machine opens its waiting ports within half a minute of the install, with no restart. `lighter helper status` and `sudo lighter helper uninstall` do what they say.
- **`lighter doctor` says why a port is closed.** A port waiting for the helper says so and how to install it; a port something else holds says that; neither is confused for the other.
- **The helper no longer bridges LAN mode.** A release has done that itself since 0.12.5, with Apple's VM Networking entitlement. Installing or removing the new helper removes the old one (`dev.lighter.bridge`), and `sudo lighter lan enable` explains this instead of installing anything. A build of your own, which cannot hold the entitlement, starts without LAN mode.
- **Each minute in the 2 MiB mode ends with everything free returned at once.** For two seconds the guest reports every free run down to 32 KiB, then goes back to 2 MiB, so freed memory waits a minute at most however busy the containers look. Measured on an M1 under a load that frees 150 MiB a minute in short runs, each of these drains gave the Mac 75 to 150 MiB, for 3 seconds of CPU in fifteen minutes, with installs as fast as before.

## Upgrading

On Homebrew 7, trust the tap once first:

```
brew trust fieldwork-ai/tap
brew upgrade lighter
lighter restart
```

If `brew services` starts lighter, `brew services restart lighter` instead of `lighter restart`. Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

If you publish a port below 1024 on one address, `sudo lighter helper install` once. If you installed the old network helper (`sudo lighter lan enable`, before 0.12.5), `sudo lighter helper uninstall` removes it, or the install replaces it.

## Also

- The guest's agent is rebuilt; the kernel and the Alpine packages are 0.13.10's. The data epoch remains **1**.
- `lighter.churn_order=<n>` on the guest's command line (`LIGHTER_CMDLINE_EXTRA`) sets the order the guest reports at while its containers work, for comparing.
- Gate m3-publish checks a port below 1024 on loopback, through the helper when one serves the build and waiting for it otherwise.
