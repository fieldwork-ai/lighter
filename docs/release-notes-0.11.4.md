# lighter 0.11.4

Two fixes reported against 0.11.3: a container running as a user other than root can create read-only files and directories in a shared folder, so git works there, and file watchers are no longer capped at the watch limit of a 2 GiB machine.

## What changed

- **Read-only files and directories can be created as a non-root user** ([#48](https://github.com/fieldwork-ai/lighter/issues/48)). In a folder shared from the Mac, a container running as a user other than root could not create a file without write permission for its owner (0444, 0555, 0400) or such a directory (`mkdir -m 555`): the create failed with "Permission denied" after the file had already appeared on the Mac, and trying again said it already existed. git writes every object it stores that way, so `git add` and `git commit` failed in a repository on the share and left empty `tmp_obj` files behind. `cp` of a read-only file failed the same way, as did `chown` of a read-only file even as root. All of these now work, and a create that fails leaves nothing behind.
- **File watchers are no longer limited by the memory the machine starts with** ([#49](https://github.com/fieldwork-ai/lighter/issues/49)). Linux sets how many files can be watched from the memory it boots with, and with `resources: cooperative` lighter's machine boots small and grows later, so the limit stayed at about 15,800 watches however much memory the machine had. Two Next.js dev servers on a monorepo used all of it, and the next one failed with "OS file watch limit reached". The machine now allows 524,288 watches and 1,024 watchers on every boot. A watch costs memory only once something makes it.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script. Containers with a restart policy come back by themselves; start the others again, as after any restart.

## Also

- Linux remains **6.18.52** (kernel 6225825f, unchanged from 0.11.3). The data epoch remains **1**.
