# macOS socket EOF investigation

The expanded Docker stream gate found intermittent `docker exec -i` hangs:
3 failures in 820 M1 checks and 1 in 820 M5 checks. Focused M1 probes reproduced
23 failures in 1000 mixed 23-byte/2-MiB stdin echoes. These are failures, not
passing qualification evidence. Instrumentation established that:

- Docker CLI called `CloseWrite` on `*net.UnixConn`, returning nil.
- The matching macOS server socket had `SS_CANTRCVMORE`, zero receive bytes,
  and Lighter's reader was blocked in `recv`, before forwarding shutdown.
- Guest credit was available, all input bytes had been forwarded, and neither
  guest half was closed. There were no pending host response buffers.
- 1000 equivalent instrumented CLI commands directly inside Linux passed.

The included C programs contain no Lighter or Docker code. On macOS 26.6.2,
100,000 socket-pair exchanges reproduce a blocking read timing out although
an immediate nonblocking peek returns zero (EOF). The M5 records two such
host reads and one peer read; its two additional peer timeouts with peek -1 can
follow the host's stalled turnaround and is not independently evidence of a
lost EOF. The M1 records one peer read with peek zero. The equivalent
nonblocking-receive/readiness-wait program passes 100,000 exchanges per host.

Build with `cc -O2 -pthread blocking.c -o blocking` (likewise `readiness.c`),
then run each binary with `100000`. The 200-ms timeout is a diagnostic failure
bound, not production behavior. Exact reproduction counts depend on timing;
zero failures in a short baseline run does not establish absence of the race.

Darwin's [soreceive implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/uipc_socket.c)
checks EOF before a protocol callback that can drop the socket lock, then
checks buffer availability before sleeping. That ordering is consistent with
the observations; it is a source-level explanation, not kernel tracing of the
installed OS. The production reader uses `MSG_DONTWAIT`, then an indefinite
`poll(POLLIN)` only on EAGAIN, and retries receive after readiness. It neither
changes the duplicated writer's blocking mode nor introduces periodic timers.
The diagnostic Lighter build passes 1000 mixed stdin commands on M1.

The initial C experiment also hit a distinct, deterministic teardown issue:
Darwin `shutdown(SHUT_RDWR)` returns ENOTCONN if receive is already closed,
without closing write. The corrected programs close write explicitly. Lighter
now attempts both directions separately on abort, with a regression asserting
that the peer gets EOF even while the local descriptor remains open.

Final runtime qualification is recorded separately; these diagnostics alone do
not qualify a release artifact.
