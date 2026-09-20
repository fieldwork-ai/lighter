# macOS Local Network privacy silently cuts containers off from the LAN

Status: diagnosed on the M5 on 2026-09-20, confirmed from the permission list, not yet fixed. Four changes proposed for 0.7.1, below. The symptom is a container that opens a connection to a device on the user's own network and never receives a byte, with nothing logged anywhere, while the Mac's own shell reaches the same device instantly.

## What it looks like

A Reolink camera at `192.168.1.120` fed Frigate in a container for twenty minutes, then stopped, and every later attempt from a container returned nothing. The camera was idle, healthy, and answering the Mac throughout. Measured, in both directions, with the camera holding no other connections:

| from | to | result |
|---|---|---|
| Mac shell | camera:554 | `RTSP/1.0 200 OK` |
| container | camera:554 | no reply; ffmpeg reports "Invalid data found when processing input" |
| container | gateway `192.168.1.254`:80 | `HTTP/1.1 200 OK` |
| container | the Mac's own LAN address | works |
| container | the internet | works |
| container | a relay on the Mac forwarding to camera:554 | works, pulls frames |

The connection appears to succeed because the guest agent accepts locally before dialing out, so `nc -z` reports every port on the host open, including ports that are closed. Only data movement distinguishes the states, which is why a reachability check is worse than useless here.

Two things this is not. It is not the stream path: the same bytes cross it fine to the gateway and to a relay one hop away. The `sockmap join failed … Not supported (os error 95)` lines in `machine.log` are unrelated — they date from 2026-09-06 and do not increment on a failing connection.

## Why

macOS Local Network privacy denies a **bundled app** unicast access to other devices on the local network until the user allows it. Three details make the failure look like a networking bug rather than a permission:

- **The gateway is exempt.** A container reaching the router therefore proves nothing, and the router is the first thing anyone tests.
- **Command-line tools under Terminal or ssh are not subject.** Every check run from a shell succeeds whatever the app's permission is, including a hand-rolled TCP relay, which is how the relay appeared to "prove" the data path was fine.
- **Without `NSLocalNetworkUsageDescription` in the bundle's `Info.plist` there is no dialog.** The user is never asked and never told; traffic simply goes nowhere. lighter declares no such key today, and the user confirmed no prompt appeared.

lighter's machine runs as an app bundle under a launchd login agent from 0.5.4 onward, so it is subject. A VMM started by hand from an ssh session is not, which is exactly why the camera worked all morning against a binary started that way and broke the moment an upgrade had launchd relaunch it.

## The part that makes it recur

**The permission is keyed on the app's identity, and lighter mints a new one on every release.** The bundle is created and re-signed at `~/.lighter/…/lighter.app` on each start, under a path carrying the version, so each release is a new app as far as macOS is concerned. System Settings › Privacy & Security › Local Network on the M5 lists **four separate `lighter` entries**, all switched on — four identities accumulated across versions. The other session found records for the 0.5.2 and 0.5.3 paths and none since 0.5.4.

So this is not a one-off. Every upgrade hands the machine an ungranted identity, and until macOS grants it, every container on the box loses the LAN with no prompt, no error and no log line. Anyone running a camera, a printer, a NAS or a Home Assistant install against lighter hits it on upgrade day and has no way to attribute it.

## A nuance on the identity, from the other session

macOS keys this permission on the app's code-signing identity where it has one. A Developer ID-signed bundle's designated requirement is its bundle identifier plus the team, path-independent, so a release installed by the install script (run from `~/.lighter/releases/<version>/share/lighter/lighter.app`, Developer ID signature intact) should carry one grant across upgrades; what mints a new identity per build is the ad hoc re-signing of a **source build** at `~/.lighter/lighter.app` (`codesign --sign -`), whose requirement falls back to the code hash. Of the four entries in the list, the ones this box could see stored paths for 0.5.2, 0.5.3 and the pre-0.5 layout, which fits ad hoc or early-release identities rather than four Developer ID releases. Whether item 3 below is already true for release installs is one toggle away: after the release is granted once, an upgrade to the next release should stay granted; if it does not, the requirement is not what macOS is keying on and the bundle needs a fixed path as well. Either way items 1, 2 and 4 stand.

## Proposed for 0.7.1

1. **Declare `NSLocalNetworkUsageDescription`** in the bundle's `Info.plist`, so there is a prompt at all and it carries lighter's own words.
2. **Trigger the prompt deliberately at `lighter start`**, the way Apple recommends, with a short Bonjour browse from the VMM, so the user is asked once up front rather than discovering it through containers that fail silently.
3. **Give the bundle a stable identity.** A fixed `CFBundleIdentifier` and a version-independent bundle path, so one grant survives every upgrade instead of each release starting denied and adding a row.
4. **A `lighter doctor` row** that connects to a LAN peer which is *not* the gateway and reports `local network: allowed`, or `denied; allow lighter in System Settings › Privacy & Security › Local Network`. It must name the peer it tested: the gateway's exemption is precisely what hides this, so a check against the gateway would pass while every other address failed.

## Reproduction

With the permission denied for the running identity, from a Mac whose shell can reach the peer:

```bash
# Nothing comes back, while the same request from the Mac's own shell answers 200 OK.
docker run --rm alpine:3.22 sh -c \
  '(printf "OPTIONS rtsp://<lan-peer>:554/ RTSP/1.0\r\nCSeq: 1\r\n\r\n"; sleep 2) | nc -w 5 <lan-peer> 554 | head -1'

# The control: the gateway is exempt and answers either way.
docker run --rm alpine:3.22 sh -c \
  'printf "GET / HTTP/1.0\r\n\r\n" | nc <gateway> 80 | head -1'
```

Toggling lighter off in the Local Network list should reproduce the failure on demand; the decision store itself needs root or the UI and cannot be read from a normal shell.

## Timeline, as evidence

| time | event | container → camera |
|---|---|---|
| 11:10 | VMM started by hand from an ssh session, 0.7.0 built locally | works |
| 11:27 | still that VMM | works |
| ~13:12 | 0.7.0 release installed; launchd relaunches the machine as a new bundle identity | fails |
| 13:30–13:45 | repeated tests, relay built as a workaround | fails direct, works via relay |
| ~14:00 | VMM run once from an ssh session, then relaunched by launchd | works |
| 14:20 | verified three times in a row on the launchd-managed machine | works |
