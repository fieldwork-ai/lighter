# lighter 0.11.6

Containers can bind from external drives, and from the Mac's temporary folders, at the same paths as on the Mac (#52). A bind from a folder lighter does not share is no longer silently empty.

## What went wrong

lighter shared your home folder and nothing else. A bind from anywhere outside it, such as `-v /Volumes/T9/media:/media`, gave the container an empty folder that dockerd had made inside the machine, with no error anywhere: the container started and found nothing. The same happened with `$TMPDIR`, which on the Mac is under `/var/folders`, and so with any tool that binds a temporary directory. Sharing more meant editing `config.json` by hand, and a shared drive that was not connected stopped the machine from starting.

## What changed

- **`/Users`, `/Volumes` and `/var/folders` are shared**, at the same paths as on the Mac. These are Docker Desktop's defaults, without `/tmp`. An existing configuration that shares the home folder now reads as these, keeping any other folder it lists.
- **A drive is there whenever it is connected**, including one plugged in while the machine runs, APFS or exFAT.
- **A drive ejects while the machine runs**, from Finder, `diskutil eject` or `hdiutil detach`, once no running container is using it. lighter keeps files open on the Mac on the guest's behalf, and macOS refuses to eject a volume while anything has a file on it open; lighter now closes what it holds on a volume when macOS asks to eject it.
- **A file a short-lived container wrote is no longer left open on the Mac.** If the guest forgot the file before lighter had finished creating it, which is ordinary when the container exits straight away, lighter held it open until 2,048 others had taken its place.
- **`lighter config --share <path>` and `--unshare <path>`** change what is shared, then `lighter restart`.
- **A shared folder that is not there**, such as a drive that is unplugged, is left out when the machine starts instead of stopping it, and `lighter doctor` names it.
- **`lighter status` and `lighter doctor` name every bind mount from a folder on the Mac that isn't shared**, with the container and the fix. `/tmp` is the machine's own rather than the Mac's, so for a bind from it they suggest your home folder or `$TMPDIR`.
- **`lighter config --metal` and `--video` are applied.** Before, they were accepted and ignored.
- **All of a machine's shares draw on one budget of open files on the Mac.** Each share used to keep its own, so more shares could have held more of the Mac's open files than one machine should.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script. Containers with a restart policy come back by themselves; start the others again, as after any restart.

## Also

- Linux remains **6.18.52** (kernel 6225825f, unchanged). The data epoch remains **1**.
- Docker still starts a container whose bind lighter cannot serve. Refusing the start, as Docker Desktop does, means lighter answering Docker's API itself, and is a change for another release.
