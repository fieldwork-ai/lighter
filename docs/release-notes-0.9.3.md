# lighter 0.9.3

A container can `chown` a shared directory to another user, and that user can then work in it: images that drop root, Frigate's next release and every database among them, run on lighter shares.

## A container's chown is recorded, not applied

lighter runs as your Mac user and shows that user as root inside the guest. It cannot give a Mac file to anyone else, so until now every `chown` a container made on a shared directory failed with "Operation not permitted", and the files stayed root's. That breaks any image that starts as root, hands its volumes to an unprivileged user and then drops to it. Frigate's development branch does exactly that (its `fix-ownership` step, then the `frigate` user), and on lighter it fell into safe mode, unable to write its own database. Postgres with a bind-mounted data directory refused to start ("data directory has wrong ownership"). A container running as a non-root user could not create anything in a share at all.

Now a container's `chown` is recorded on the Mac file and reported back inside the guest, while the file stays yours on the Mac. The record uses the extended attribute Docker Desktop uses for the same purpose, `com.docker.grpcfuse.ownership`, in its format, so ownership set under one runtime reads the same under the other. `chown` back to root clears it. A file a non-root process creates is its own. On an M5, Frigate from its development branch ran as its `frigate` user with no `FRIGATE_RUN_AS_ROOT`, wrote its database and its recordings, and kept doing so across a restart of the machine; Postgres 16 initialized its data directory as `postgres` (uid 70, mode 700) and served queries.

A chowned file shows an `@` in `ls -l` on the Mac, for the attribute, as it does with Docker Desktop. The directory holding it, and the root of the share (your home directory, by default), get a second attribute, `sh.lighter.ownership`, which is how lighter knows where records can be.

Reading a record costs about nine times a plain `stat` on APFS, so lighter reads one only where it can exist: in a directory something has been chowned in, and only on a share that has any. A share where nothing has been chowned costs exactly what it did in 0.9.2, and a package tree beside a chowned volume pays one read per directory, not per file. Gate `m4` chowns a volume from one container and works in it as uid 1000 from another.

## Also

- A container asking the size of an extended attribute before reading it, as `getfattr` does, got "Numerical result out of range": an empty buffer reached macOS as a non-null pointer, which it answers with ERANGE. Fixed, with a test.
- A container listing a file's extended attributes no longer sees macOS's own (`com.apple.*`, such as the `com.apple.provenance` macOS puts on every file lighter writes). They mean nothing to a Linux program, and once listing worked, `cp -a` copied provenance onto every file it made, which cost the copy-tree benchmark up to a tenth of its time. Asked for by name they are still there.
- Linux remains **6.18.52**, with guest patches 0036 to 0042; the data epoch remains **1**.
