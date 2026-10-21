# lighter 0.13.8

When the Mac runs out of room for open files, lighter now gives back what it holds, and it never holds a third of the Mac's files to begin with. A Mac whose file table fills fails opens in every program and logs its user out. On the Mac in #95 lighter held about 160,000 of the 491,520 the Mac allows, and gave none of them back when another program filled the rest. Thanks to @petrfilip for the report, the diagnosis and the fix this release builds on (#95, #96).

A file a container has open now stays the file it opened when a Mac program deletes it or saves over it, as it does on OrbStack, Docker Desktop and a Linux disk. Folders shared from the Mac also take sockets, device nodes and unusual permissions as a Linux disk does. And hardware video decoding works for live cameras: it stopped after one frame, and cost a guest CPU what decoding in software did (#98). Thanks to @stuartb55 for the report, the workaround and the measurements behind both.

## What went wrong

**lighter could take a third of the Mac's files** (#95). lighter keeps a descriptor for each file and folder a container has looked at, as a cache, up to three quarters of what macOS lets one process open. But every program's open files come out of one table for the whole Mac, and macOS lets one process open a third to a half of it, so lighter's cache alone could take a third of the table. When another program filled the rest, every open on the Mac failed and the user was logged out.

**When the table was full, lighter gave nothing back.** A container's open failed with "too many open files in system", and lighter kept every descriptor it had. A file a container had just created could be lost: lighter answers a create at once and makes the file a moment later, and when the full table refused it then, no request was left to try again. The log said `an acknowledged create failed to apply errno=23`.

**A quiet share could starve a busy one.** A machine's shares (`/Users`, `/Volumes`, `/var/folders`) count against one budget, but each lets go only of its own descriptors, and only while a container uses it. A share that had filled the budget and gone quiet kept it, and a busy share beside it had to open every file afresh.

**A file a container had open could disappear, or become another file.** When a Mac program deleted a file a container had open, the container's next read failed. When a Mac program saved over it, as most editors do (writing a new file and renaming it into place), the container could read the new file's contents through the old one.

**lighter held a descriptor for every file a container had looked at**, though it never needed one: 102,000 of them on a Mac Studio during a large install. And a file registered without one, every file on a NAS and every new one once the budget was full, was never forgotten, so lighter's memory of them only grew.

**Some things a container can do on a Linux disk failed in a shared folder.** Binding a UNIX socket there failed (`EPERM`), as an app server listening on `unix:/app/app.sock` in a mounted project does, and so did making a device node. A directory made without read permission for its owner (`mkdir -m 0151`) failed with `EACCES` though it had been made. A FIFO deep enough failed with `ENOSPC`. A chown kept a file's setuid bit.

**Hardware video decoding stopped after one frame for a live camera** (#98). Over RTSP, ffmpeg starts reading frames before the first keyframe has arrived, and the decoder took the size the keyframe brought for a change of size, which it ends with a "last" frame: ffmpeg stopped there. Frigate's cameras restarted ffmpeg forever at 0 fps on `h264_v4l2m2m` and `hevc_v4l2m2m`.

**Decoding in hardware cost a guest CPU what decoding in software did** (#98). Each frame was decoded while the guest's vCPU waited for the answer, and the guest counted the wait as ffmpeg's: a tenth of a core for a 2688x1512 camera at 30 fps.

## What changed

- **lighter's cache takes a fifth of the Mac's file table at most**: 98,304 on a Mac Studio whose table is 491,520, where it was 161,280. A stock Mac is unchanged, at 6,144.
- **A full table gets room back.** When an open fails because the Mac's table is full, every share lets go of everything it holds idle, and lighter holds half as much until the table has room again: each minute the table is no more than three quarters full, it holds twice as much, back to where it was. A request that only reads is served again, so the container sees a moment's wait. A request that changes something sees the error once, as on a NAS that refuses. `machine.log` says when the table was full and when it had room again.
- **A file a container created is made, even if the table was full at that moment.** lighter lets go of what it holds and makes the file again.
- **A machine's shares share their budget.** A share that holds less than half of it when it is full has the other shares let go of what they have not used lately.
- **A file a container has open stays the file it opened.** The guest tells lighter which files stay open, a few milliseconds after they open, and lighter holds each until it is closed. Deleted or saved over on the Mac, it reads what it opened; a file opened and closed at once, as nearly every file is, is never mentioned, so this costs nothing measurable. One deleted on the Mac in the first milliseconds after it opens reads as gone (`ESTALE`), never as another file.
- **lighter holds no descriptor for a file**, only for folders: 30,000 at that install's peak, the same speed or faster in every benchmark. Files it remembers are forgotten when the container forgets them.
- **A change made on the Mac reaches containers sooner.** lighter asks macOS to report it within a millisecond, where it allowed ten: a container that saw a file the Mac wrote second could read the old contents of the one it wrote first.
- **Sockets and device nodes work in a shared folder.** On the Mac each is an empty file; in containers it is what was made. A directory or file made with permissions the Mac user cannot use is made usable on the Mac, with the container's permissions kept for containers; read-only files stay read-only on the Mac. FIFOs work at any depth, a chown drops setuid and setgid, and a file's recorded owner follows it when it moves.
- **A file renamed over one a container had just created no longer loses it.** lighter answers a create at once and makes the file a moment later; a rename in between landed first, and the create then failed and lost what had been written (the log said `an acknowledged create failed to apply`). A rename over a directory whose only contents were just created is refused as not empty, as on Linux, and a FIFO, socket or device node made where one was just removed is made.
- **Live camera streams decode in hardware**, from the first keyframe on, for H.264 and HEVC. The `noise` bitstream filter workaround is no longer needed.
- **Hardware decoding runs off the guest's CPUs.** For one 2688x1512 camera in Frigate's detection pipeline, ffmpeg's CPU in the guest went from 100 to 30 ms a second for H.264 and from 62 to 37 for HEVC, against 190 to 250 for decoding in software.
- **A NAS's "too many open files in system" is still the NAS's**, unless the Mac's own table is nearly full, and is handled as in 0.13.7.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- Linux remains **6.18.52**. Guest patch 0057 is new (the guest says which files are open), and 0004 refuses to clone a file onto itself, which it could not do correctly. The data epoch remains **1**.
- A new qualification gate, m21, holds shared folders to a local disk: the open-file probes, fsx with every operation, and pjdfstest against a list of what a shared folder fails by design. pjdfstest's failures went from 3,077 to 525, the same 525 every run, against 2,772 on OrbStack; what remains follows from a Mac file belonging to whoever asks, as on Docker Desktop.
