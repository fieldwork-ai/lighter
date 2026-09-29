# lighter 0.10.3

A container that runs as its own user can use a bind mount without a `chmod 777` first ([#36](https://github.com/fieldwork-ai/lighter/issues/36)).

## Bind mounts for a container's user

Files in a shared folder belong to your Mac user, which a container has never heard of. lighter used to show them as root's, so a container started with `--user 1000:1000` (or `user:` in compose) was neither their owner nor in their group, and only the "other" permission bits applied: a 770 folder refused it, a 660 file could not be read, and passing the UID and GID through did not help. Only `chmod 777` did.

lighter now shows such a file as belonging to whoever is asking, as Docker Desktop and OrbStack do. A container running as 1000 sees its bind mount as 1000's, a container running as root sees it as root's, and two containers with different users on the same folder each see their own. Each of them can read, write, `chmod` and `touch` what the mode bits give its owner.

- On the Mac nothing changes: the files stay yours, and what a container creates is yours too.
- A container's `chown` still records a real owner (0.9.3), which other users are then held to, and a `chown` back to root gives the file back to whoever asks.
- On 0.10.2, the workaround is to chown the folder from a root container once: `docker run --rm -v ./folder:/folder alpine chown -R 1000:1000 /folder`.

## How

The file server flags every attribute it reports for a file your Mac user owns and no container has chowned. A new guest kernel patch (0046) answers `stat`, the permission check, `chmod`, time sets and a `chown` to oneself for a flagged file as if the caller owned it. The answer is per caller, not stored in the attributes the guest caches for everyone. The inode still says root, which is what the rest of the kernel sees, as before (`docs/architecture.md`, "Ownership").

Gate m4-fs runs three users in turn, 1000, 2000 and root, against an unchowned 770 folder with a 660 file. Each reads, writes, `chmod`s and `touch`es it and sees itself as the owner. The gate also checks that a second user still cannot write a 644 file a container chowned to 1000.

## Also

- Linux remains **6.18.52**, with patch 0046 added. The root filesystem is 0.10.2's, byte for byte (rootfs 5d4b5934), and the data epoch remains **1**.
