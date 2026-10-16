# lighter 0.13.7

A container walking a large library on a NAS no longer runs the NAS out of open files. lighter held a file open on the server for every file a container had looked at, and a photo library has more files than a NAS will keep open: once it ran out, every open on the share failed, for containers and the Mac alike. Thanks to @skwiz0d for the report and the log (#93).

## What went wrong

**A network volume was held to the Mac's limit, not its server's** (#93). lighter keeps a descriptor for each file and folder a container has looked at, so that a file keeps its identity through renames, and it lets go of the coldest when the Mac's own limit comes near: about 138,000 on a Mac whose limit was raised. On the Mac's own disk that costs nothing but the descriptor. On an SMB or NFS volume each descriptor is also an open handle on the server, and the server has a limit of its own, shared by every client of the share, that is far lower than a photo library. Immich scanning an external library held one handle on the NAS for every photo it had seen, and the guest, with memory to spare, never let go of them. Once the NAS was out of handles, every open on the share failed: Immich logged `EMFILE: too many open files`, and folders listed empty in containers and in Finder. Only a restart of lighter gave the handles back. Up to 2,048 more were held by the cache of files lighter keeps open for reading, a cache sized for the Mac's own disk too.

## What changed

- **A network volume holds a few hundred handles at most, whatever its size.** A file on it holds nothing between requests: it is reached through its folder and its name, and opened only to be read. Its folders keep a descriptor each, up to 128, and the cache keeps up to 64 of its files open for reading. A walk of 4,000 photos held under 200 descriptors on the share, where 0.13.6 held 6,150; a walk of 40,000 on a Samba server limited to 2,048 handles held at most 194 of them, and every photo read back.
- **A server that refuses an open anyway is let go of and asked again.** If a NAS has fewer handles to give than that, it refuses an open with "too many open files". lighter then lets go of everything it holds idle on that volume, holds half as much there from then on, and serves the request again, so the container sees nothing but a moment's wait. A request that changes something is not served twice: it may have done part of its work, so the container sees the error once, and its next attempt finds room. `machine.log` says when a server refuses and what lighter holds there afterwards.
- **Files on a network volume are opened by their whole path**, as they were in 0.13.6. Opened by name under their folder, the first read of each photo took 43 ms on a test build, against 0.2 ms by path.

Reading and listing a share are as fast as in 0.13.6. Nothing changes on the Mac's own disks.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- Linux remains **6.18.52**, with guest patches unchanged since 0.13.6. The data epoch remains **1**.
