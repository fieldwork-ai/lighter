# lighter 0.12.4

Databases start with their data in a shared folder or on a drive, a container's nested mounts stay in place when the Mac changes the folders they sit in, and connections a server resets no longer pile up until containers cannot connect out.

## What went wrong

**MariaDB could not start with its data in a shared folder or on a drive** (#69, Borg Backup Server). lighter read the open flags a container passes in the wrong numbering: `O_DIRECT`, which MariaDB's storage engine opens its files with, reached the Mac as "open a directory", which the Mac refuses. The database could not create its files, and the container restarted forever: ten times in its first 75 seconds. A file preallocated straight after it was created also failed, with "Stale file handle".

**On exFAT and FAT drives, as most drives are sold, a container's `chmod` did not stick.** Those drives keep no permissions, and the Mac reports every file on them as its owner's alone (`rwx------`). A service whose user has to reach a directory another user owns could not, and a rename that must not replace a file, which ClickHouse uses, failed there with "Operation not supported".

**A container lost the bind mounts nested inside another while it ran** (#70), when the Mac touched a directory they sit in or changed its extended attributes. `docker inspect` still listed them; the container saw empty directories, and what it wrote went to the Mac's folder underneath.

**After a restart, every owner a container had set in the home folder or on a drive read as root**, until some container ran `chown` again. A database whose image checks its data directory's owner before changing it could refuse to start after `lighter restart`. The ownership was kept all along; lighter looked for it only once a `chown` had happened since it started.

**Every outbound connection its server reset kept two descriptors in lighter's machine** (#63). On a home server running Home Assistant, Frigate and Zigbee2MQTT, lighter's proxy ran out of all 65,536 in about six hours, and containers' connections out were refused until a restart.

## What changed

- **`O_DIRECT` opens work.** MariaDB initializes and runs with its data in a shared folder. Borg Backup Server, which failed to start on 0.12.3, starts and serves its web interface with its data in the home folder and on an exFAT drive.
- **On exFAT and FAT drives, a container's `chmod` is kept**, in the record lighter already keeps for a container's `chown`, and every container sees it, after a restart too. The drive itself is untouched, and new files start as the Mac reports them. A rename that must not replace works there, as it does with Linux's own exFAT driver.
- **Owners a container sets are kept across restarts**, in the home folder and on drives alike.
- **A change on the Mac never takes a container's mounts away.** What the Mac removes, renames or creates is still seen at once.
- **A connection its server resets is let go.** After 32 of them, lighter's proxy holds the 10 descriptors it held before; 0.12.3 held 74.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- The Mac's own disk and exFAT drives treat names that differ only in case as one, so a container that writes two such names into a shared folder finds a single file. ClickHouse does, inside Borg Backup Server, which then runs with its catalog features off. That is the drive, with lighter as with Docker Desktop: keep such data in a Docker volume, or on a case-sensitive APFS volume, where Borg Backup Server runs in full.
- Linux remains **6.18.52** (kernel f4dfc278). The data epoch remains **1**.
