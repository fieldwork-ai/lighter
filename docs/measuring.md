# Measuring this thing

Most of the work on a shared filesystem is measurement, and most of the wasted
work is measurement done badly. This is what was learned doing it, in the order
it cost the most time.

## Pick the instrument before the question

| | resolves | costs | answers |
|---|---|---|---|
| `make quick` | pass/fail | 15s | did I break it |
| `make latency` | ~2 us | 15s | did this change help, and on which side |
| `make bench` | ~5% | 30s | did it land, roughly |
| `make bench-full` | ~5% | 20 min | the published table |
| `make gates` | pass/fail | ~45 min | is it still correct |

The first three are phase one, for iterating: run them on every change. The
last two are phase two, for a group of changes already believed to be good.

`make bench` runs the real cases against a fixture a tenth the size — 128
packages and 6,246 files rather than 1,232 and 66,213. Same shape of work, a
tenth of the wait. It tracks the real thing on the write cases (54% of native
against 52%) and **not** on the read cases (91% against 844%), because sixty-two
megabytes fits in the Mac's own page cache and nine hundred does not, so there
is no advantage left to win. It is a regression detector there, not a proxy.

The workload cases have a standard deviation of 2.4%, measured across twenty
runs under configurations that turned out to be equivalent. Three repetitions
resolve a 5.5% difference; ten resolve 3.0%; resolving half a percent would
take about three hundred and sixty.

**So an effect below five percent cannot be found with a workload case, however
many times it is run.** Two whole afternoons went into learning that: one
comparing packed and split virtqueues, which differ by 0.52%, and one comparing
worker-pool sizes, which differ by less than the spread. Both questions were
answerable — by the latency probe, in a minute.

## More samples do not fix a busy machine

Interference is not zero-mean noise. It is a bias, and averaging a biased
estimator gives a precise wrong answer. The same `create+close` reads 43.4
microseconds on a quiet machine and 75.6 on one with a second hypervisor
running, and no number of repetitions moves the second towards the first.

Three things that do help:

- **A control in the same run.** Every latency case is measured against the
  share *and* against the guest's own disk, alternating. If the control moved,
  the guest side changed; if only the share moved, we did. It does not cancel
  interference — the two paths do not share a bottleneck — but it says where a
  change landed, which is usually the question.
- **Interleaving.** Alternate the two arms boot by boot rather than running all
  of one and then all of the other, so drift hits both equally.
- **Another machine.** `scripts/provision-bench-host.sh` sets one up. Numbers
  from a different Mac are not comparable with numbers from this one, but they
  are comparable with each other, which is all a comparison needs.

## Measure the shape the workload has

`write-4k` writes four kilobytes in one syscall and says write-back caching
does nothing. `write-chunked` writes sixty-four kilobytes in eight, which is
what a decompressor does, and says write-back caching collapses eight requests
into one — and is slower anyway, for reasons the first case cannot see.

The same mistake in the other direction: every case was single-threaded until
`create-parallel` was added, so a lock on either side of the boundary was
invisible. When it was added it immediately found two, one in our code and one
in the guest patch.

## Let the instrument report its own uncertainty

`latency.sh` prints a spread column. A change smaller than the spread has not
been measured, however confident the difference of two medians looks. Its own
variation is boot-to-boot rather than sample-to-sample, so `OPS` is already far
past diminishing returns and `REPEAT` is the knob that matters; the first boot
is discarded as a warm-up.

## Find out where the time goes before deciding where to spend it

`LIGHTER_FS_STATS=1` gives the server's opcode histogram — count and mean
microseconds per operation, sharded across cache lines so that watching does
not change what is watched. `LIGHTER_FS_TRACE=N` logs the first N requests in
order.

The histogram says a package install is 636,000 requests of which 66,000 are
creates at 39 microseconds each, two thirds of all host time. The trace says a
create is three round trips — `getattr` of the parent, `lookup`, `create` —
and that the first two are the kernel asking for things it has been told. One
number says stop tuning the transport; the other says what to patch instead.

## Correct the record when it turns out to be wrong

Three explanations in this repository were confidently stated and false: that
`npm ci` clones from its cache (it holds tarballs), that write-back caching had
no effect (it has a large one, in the wrong direction), and that a round trip
costs fifteen microseconds (it is one to two, since the guest stopped trapping
to submit).

Each had been reasoned about rather than measured, and each survived because it
was plausible. They are corrected in place with a note saying what was wrong,
because the failure mode is more useful than the fact.
