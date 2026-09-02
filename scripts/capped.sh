#!/usr/bin/env bash
# Runs a command under a hard wall-clock cap, killing its whole process
# group when the cap expires.
#
#   scripts/capped.sh 600 ./benchmarks/run.sh --target lighter ...
#
# The benchmark harness caps each case, but a hang can sit in a phase no
# per-case cap covers — a warm-up install, a fixture materialization — and
# an unsupervised run then waits for a morning that finds it exactly where
# it stopped. This is the outer bound: nothing launched through it outlives
# its number, whatever it is doing. Exit status 124 says the cap fired.
set -u
[ $# -ge 2 ] || { echo "usage: $0 <seconds> <command...>" >&2; exit 2; }
cap="$1"; shift
# A fresh process group (macOS has no setsid) so the whole tree — the
# harness, its docker clients, its VMM — can be killed at once.
perl -e 'setpgrp(0, 0); exec @ARGV or die "exec: $!"' -- "$@" &
child=$!
fired="$(mktemp -t capped)"; rm -f "$fired"
# The watchdog keeps only stderr, for the CAPPED line: a `sleep` orphaned
# on our stdout would hold a pipe open for the whole cap after the command
# had finished. The marker file, not the watchdog's liveness, says whether
# the cap fired — the watchdog is still alive during its grace period.
( exec </dev/null >/dev/null; sleep "$cap"; if kill -0 "$child" 2>/dev/null; then touch "$fired"; echo "CAPPED after ${cap}s: $*" >&2; kill -TERM -- -"$child" 2>/dev/null; sleep 5; kill -KILL -- -"$child" 2>/dev/null; fi ) &
watchdog=$!
wait "$child"; status=$?
pkill -P "$watchdog" 2>/dev/null; kill "$watchdog" 2>/dev/null; wait "$watchdog" 2>/dev/null
if [ -e "$fired" ]; then status=124; rm -f "$fired"; fi
exit "$status"
