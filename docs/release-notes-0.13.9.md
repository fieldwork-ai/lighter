# lighter 0.13.9

Containers now reach the devices on your Mac's network however lighter is started at login: by `brew services` or by a launchd agent of your own, as by `lighter install`. Started by `brew services`, lighter reached the Mac's router and nothing else on its network, and no setting in System Settings could change that (#101). Thanks to @stuartb55 for the report, which named the launcher, the doctor's line and the workaround, and made the cause one experiment away.

## What went wrong

**Started by `brew services`, lighter could not reach the Mac's network** (#101). macOS asks before an app talks to other devices on the Mac's network, and gives the permission to the app: for lighter, `lighter.app`, which says why it needs it and is listed in System Settings › Privacy & Security › Local Network. lighter's own login agent (`lighter install`) runs `lighter.app`. The Homebrew formula's service ran the `lighter` command instead, which macOS never asks about and never allows. A container's connection to a camera, a NAS or a printer opened and was closed at once: TLS failed with an unexpected EOF, HTTP got no answer, and an NVR's RTSP stream died. `lighter doctor` said `local network denied`, and allowing every lighter entry in System Settings changed nothing.

**A container's TXT, SRV or MX lookup failed while the Mac's first nameserver was down.** lighter answers those by asking the nameservers the Mac's configuration picks for the name, but it asked only the first. When that one stopped answering, as an ISP's IPv6 nameserver did on our network while 0.13.9 was being qualified, the Mac's own resolver moved on to the next and every app on the Mac kept working; a container waited five seconds and got nothing.

**A file's link count could read one too many in a shared folder.** A name unlinked and linked straight back, as pjdfstest does and a build that replaces a hard link can, was counted twice until the unlink landed on the Mac: `stat` said three names for two. It took a busy Mac, two runs in forty.

## What changed

- **The machine is always `lighter.app`.** Started any other way, by a launchd agent or by hand, it becomes `lighter.app` before it starts the virtual machine, in the same process, so launchd still watches it.
- **`brew services` runs what `lighter install` runs**: `lighter.app`, as an interactive process, logging to `~/.lighter/machine.log`, the log `lighter doctor` and bug reports ask for. It logged to Homebrew's `var/log/lighter.log` before. The formula change reached 0.13.8 on the day of the report; `brew update && brew services restart lighter` applies it.
- **The next nameserver is asked when the first does not answer**, a second later, as the Mac's resolver does, and the first answer from any is the one a container gets. When none answers within four seconds the container is told so (SERVFAIL) inside the five it waits, rather than left to time out.
- **A link's count is the Mac's once the unlinks before it land**, as `stat` already waited for.
- **`lighter doctor` names a machine running outside `lighter.app`**, and for a denied local network says what to allow: `lighter.app`, or, for a machine started from an app's terminal, that app, whose permission macOS gives a program it starts. Terminal and ssh need none.

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
