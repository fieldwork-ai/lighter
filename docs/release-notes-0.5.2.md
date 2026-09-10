# lighter 0.5.2

Virtual disks now recover automatically when host disk space returns. If a host
write runs out of space, lighter retains the unfinished request and retries it
every three seconds instead of passing an I/O error to Linux that can abort a
filesystem journal. The VM keeps running, with no periodic space checks during
normal operation.

Independent reads continue while writes wait. Reads overlapping unfinished
writes wait for those writes to finish, and writes, flushes, discards and
zeroing preserve their order. This also avoids the sustained Docker/containerd
CPU use observed when all reads were held behind a failed write.

`lighter status` reports which disks are waiting for host space. Stopping the
VM while writes are unfinished warns that those writes can be lost. Once space
returns, pending work resumes without restarting the VM or its containers.
Applications can still time out during prolonged disk exhaustion. This change
prevents recoverable host ENOSPC from being reported as guest I/O failure; it
does not repair a filesystem whose journal has already aborted or retry
unrelated I/O errors.

The Linux kernel remains **6.18.49** and the data epoch remains **1**. Existing
configuration and container data are preserved during upgrade. Homebrew
installations continue to be managed by Homebrew.
