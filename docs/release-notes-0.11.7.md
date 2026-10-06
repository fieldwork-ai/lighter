# lighter 0.11.7

Containers no longer lose their outbound connections after a long run (#57), containers running as different users can work in the same shared folder (#55), and a non-root container can remove its own files from a sticky directory.

## What went wrong

- **Outbound connections stopped working after hours (#57).** When a container closed a connection to something that never closes its own end, as many devices and servers do with idle connections, lighter kept carrying it forever, holding four of its descriptors. A Home Assistant polling such a device used them all up overnight, and from then on every new outbound connection from every container was refused until lighter restarted.
- **One container user was locked out of what another made (#55).** What a container running as a non-root user created in a shared folder was recorded as that user's, so a container running as anyone else got only the "other" permission bits: a build container running as you and a sandbox running as 1000 each found the other's directories read-only. Neither Docker Desktop nor OrbStack does this.
- **Sticky directories refused their own files.** A non-root container could not delete a file it owned, or a Mac file, from a directory with the sticky bit (`chmod 1777`), because Linux still saw such files as root's.
- **Piping lighter's output crashed it.** `lighter status | head -1` ended in a panic.

## What changed

- **A connection whose container side has gone is let go.** lighter checks a container's side of each connection once it has been quiet for 30 seconds, and closes the connection as soon as that side no longer exists. A container that has only finished sending, and is waiting for a reply, keeps its connection for as long as the reply takes.
- **When lighter does run out of room for connections, it serves the ones it has before refusing more**, and says why in its log. This part is from [@ParalaXEngineering](https://github.com/ParalaXEngineering)'s pull request #56.
- **What a container creates belongs to whoever asks**, as a file made on the Mac already does: every container user sees it as its own and can write it. A `chown` inside a container is still recorded and kept, so images that hand their data folders to a service user, such as Frigate and Postgres, work as before.
- **Sticky directories, and the protections Linux applies inside them, treat a file that belongs to whoever asks as the caller's.** This takes a change to the guest's Linux kernel.
- **lighter's output into a pipe that closes early is dropped quietly**, and the command carries on.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script. Containers with a restart policy come back by themselves; start the others again, as after any restart.

A file a container created before this release is still recorded as its creator's. To make a folder's contents belong to whoever asks again, run `docker run --rm -v "$PWD/<folder>:/f" alpine chown -R 0:0 /f` from the folder's parent.

## Also

- Linux remains **6.18.52**, with one new patch (kernel 3ed15e47). The data epoch remains **1**.
