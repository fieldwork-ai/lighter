# lighter 0.13.9

Containers now reach the devices on your Mac's network however lighter was started: by `brew services`, by a launchd agent of your own, or with `lighter start` from any app's terminal. Started by `brew services`, lighter reached the Mac's router and nothing else on its network, and no setting in System Settings could change that (#101). Thanks to @stuartb55 for the report, which named the launcher, the doctor's line and the workaround, and made the cause one experiment away.

## What went wrong

**Started by `brew services`, lighter could not reach the Mac's network** (#101). macOS asks before an app talks to other devices on the Mac's network, and gives the permission to the app: for lighter, `lighter.app`, which says why it needs it and is listed in System Settings › Privacy & Security › Local Network. lighter's own login agent (`lighter install`) runs `lighter.app`. The Homebrew formula's service ran the `lighter` command instead, which macOS never asks about and never allows. A container's connection to a camera, a NAS or a printer opened and was closed at once: TLS failed with an unexpected EOF, HTTP got no answer, and an NVR's RTSP stream died. `lighter doctor` said `local network denied`, and allowing every lighter entry in System Settings changed nothing.

**Started with `lighter start`, lighter had someone else's permission.** macOS gives a program started from a terminal the permission of the app the terminal runs in. A machine started from Terminal reached the network because Terminal was allowed, not because lighter was. One started from an app that was not allowed reached nothing, and one started over ssh, or from a session that later ended, lost the network when that session ended.

## What changed

- **The machine is always `lighter.app`.** Started any other way, by a launchd agent or by hand, it becomes `lighter.app` before it starts the virtual machine, in the same process, so launchd still watches it.
- **The machine has its own permission wherever it is started.** `lighter start` starts it as an app of its own rather than as part of the terminal's, so it has `lighter.app`'s permission, and keeps it when the terminal, the app it runs in or the ssh session goes away.
- **`brew services` runs what `lighter install` runs**: `lighter.app`, as an interactive process, logging to `~/.lighter/machine.log`, the log `lighter doctor` and bug reports ask for. It logged to Homebrew's `var/log/lighter.log` before. The formula change reached 0.13.8 on the day of the report; `brew update && brew services restart lighter` applies it.
- **`lighter doctor` names a machine running outside `lighter.app`**, and the remedy for a denied local network is `lighter restart`, which now works from anywhere.

## Upgrading

```
brew upgrade lighter
lighter restart
```

If `brew services` starts lighter, `brew services restart lighter` instead of `lighter restart`. Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- The guest is 0.13.8's: Linux **6.18.52** and the same Alpine packages, with only lighter's agent rebuilt. The data epoch remains **1**.
- The release archive now carries `THIRD-PARTY-NOTICES`: the licences of the Rust crates and the C and C++ libraries built into lighter, and of Linux and every Alpine package in the guest. The complete source of the Linux guest, Linux with lighter's patches and every Alpine package in it, is attached to the release as `lighter-0.13.9-guest-source.tar`.
- The README's licence section is shorter, and the pull request template asks for less (#102).
