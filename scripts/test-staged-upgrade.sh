#!/usr/bin/env bash
# Exercise activation with privately signed future fixtures, never public tags.
# Arguments: current archive, future archive, signed nonbooting archive, helper.
set -euo pipefail
CURRENT="${1:?current archive}" FUTURE="${2:?future archive}" BROKEN="${3:?nonbooting archive}" HELPER="${4:?signed current helper}"
LOGIN="${LIGHTER_TEST_LOGIN:-0}"
if [ "$LOGIN" = 1 ]; then
 [ ! -e "$HOME/Library/LaunchAgents/dev.lighter.machine.plist" ]
 if launchctl print "gui/$(id -u)/dev.lighter.machine" >/dev/null 2>&1; then
  echo 'Refusing to replace an existing login service' >&2; exit 1
 fi
fi
WORK="$(mktemp -d /tmp/lighter-staged.XXXXXX)"
export HOME="$WORK/u" DOCKER_CONFIG="$WORK/u/.docker"
unset LIGHTER_HOME LIGHTER_GUEST_DIR
PREFIX="$HOME/.lighter" L="$HOME/.lighter/bin/lighter"
cleanup() {
 result=$?
 if [ "$result" -ne 0 ]; then
  find "$WORK" -maxdepth 1 -name '*.log' -exec tail -20 {} \; >&2
 fi
 if [ "$LOGIN" = 1 ]; then
  launchctl bootout "gui/$(id -u)/dev.lighter.machine" >/dev/null 2>&1 || true
 fi
 [ ! -x "$L" ] || "$L" stop >/dev/null 2>&1 || true
 rm -rf "$WORK"
}
trap cleanup EXIT
mkdir -p "$PREFIX"
printf '%s\n' '{"cpus":2,"memory_mib":2048,"disk_gib":64,"shares":[],"publish":"localhost"}' > "$PREFIX/config.json"
"$HELPER" install-archive --archive "$CURRENT" --prefix "$PREFIX"
base="$(basename "$(readlink "$PREFIX/current")")"
# This fixture is already trusted local input. Real extraction and trust were
# checked on installation; upgrade independently verifies the staged payload.
stage() {
 local archive="$1"
 mkdir -p "$WORK/extract"
 tar -xzf "$archive" -C "$WORK/extract"
 python3 - "$PREFIX" "$WORK/extract" <<'PY'
import json,pathlib,shutil,sys,time
prefix,unpack=map(pathlib.Path,sys.argv[1:])
root=next(unpack.iterdir())
version=json.loads((root/'share/lighter/lighter.app/Contents/Resources/release.json').read_text())['version']
identity=json.loads((prefix/'share/lighter/installation.json').read_text())['id']
cache=pathlib.Path.home()/'Library/Application Support/lighter/updates'/identity
cache.mkdir(parents=True,exist_ok=True)
target=cache/f'release-{version}'
if target.exists(): shutil.rmtree(target)
shutil.move(root,target)
(cache/'state.json').write_text(json.dumps(dict(automatic=False,attempted=int(time.time()),checked=int(time.time()),available=version,downloaded=version)))
print('staged',version)
PY
 rmdir "$WORK/extract"
}
if [ "$LOGIN" = 1 ]; then
 "$L" install
 for attempt in $(seq 1 120); do
  if docker -H "unix://$PREFIX/docker.sock" info >/dev/null 2>&1; then break; fi
  sleep 1
 done
 docker -H "unix://$PREFIX/docker.sock" info >/dev/null
 "$L" stop
fi
stage "$FUTURE"
python3 - "$HOME" "$WORK" <<'PYLOCK' &
import fcntl,pathlib,sys,time
home,work=map(pathlib.Path,sys.argv[1:])
state=next((home/'Library/Application Support/lighter/updates').iterdir())
with (state/'operation.lock').open('a') as lock:
 fcntl.flock(lock,fcntl.LOCK_EX)
 (work/'locked').touch()
 deadline=time.monotonic()+30
 while not (work/'release-lock').exists() and time.monotonic()<deadline: time.sleep(.05)
PYLOCK
lock_pid=$!
for attempt in $(seq 1 100); do [ ! -e "$WORK/locked" ] || break; sleep .05; done
[ -e "$WORK/locked" ]
if "$L" upgrade > "$WORK/concurrent.log" 2>&1; then echo 'FAIL: concurrent updater accepted'; exit 1; fi
grep -q 'another updater' "$WORK/concurrent.log"
touch "$WORK/release-lock"
wait "$lock_pid"
echo 'PASS: concurrent updater refused'
"$L" upgrade
future="$(basename "$(readlink "$PREFIX/current")")"
[ "$future" != "$base" ]
[ ! -e "$PREFIX/lighter.pid" ]
if [ "$LOGIN" = 1 ]; then
 if launchctl print "gui/$(id -u)/dev.lighter.machine" >/dev/null 2>&1; then
  echo 'FAIL: stopped upgrade loaded login service'; exit 1
 fi
fi
echo 'PASS: stopped upgrade remains stopped'
# Simulate interruption after selecting the next generation. Recovery must
# restore the previous selection without activating the pending update again.
python3 - "$PREFIX" "$base" "$future" "$LOGIN" <<'PYRECOVERY'
import json,pathlib,sys
prefix=pathlib.Path(sys.argv[1]).resolve()
(prefix/'upgrade.json').write_text(json.dumps(dict(previous=str(prefix/'releases'/sys.argv[2]),next=str(prefix/'releases'/sys.argv[3]),running=False,login=sys.argv[4]=='1')))
PYRECOVERY
if "$L" upgrade > "$WORK/recovery.log" 2>&1; then echo 'FAIL: recovery silently retried activation'; exit 1; fi
grep -q 'recovered interrupted upgrade' "$WORK/recovery.log"
[ "$(basename "$(readlink "$PREFIX/current")")" = "$base" ]
[ ! -e "$PREFIX/upgrade.json" ]
[ ! -e "$PREFIX/lighter.pid" ]
echo 'PASS: interrupted transaction recovers previous stopped release'
# Return to the current release for the separate running-VM transaction.
"$HELPER" install-archive --archive "$CURRENT" --prefix "$PREFIX"
"$L" start --timeout 120
D=(docker -H "unix://$PREFIX/docker.sock")
"${D[@]}" run --rm -v upgrade-data:/data alpine:3.21 sh -c 'echo survived > /data/value'
"${D[@]}" run -d --name upgrade-container --restart always alpine:3.21 sleep 3600 >/dev/null
original_pid="$(cat "$PREFIX/lighter.pid")"
stage "$FUTURE"
# A modified kernel must be rejected while the original VM keeps running.
python3 - "$HOME" <<'PYTAMPER'
import pathlib,sys
root=pathlib.Path(sys.argv[1])/'Library/Application Support/lighter/updates'
image=next(root.glob('*/release-*/share/lighter/Image'))
with image.open('r+b') as f: f.write(b'TAMPERED')
PYTAMPER
if "$L" upgrade --restart > "$WORK/tamper.log" 2>&1; then echo 'FAIL: tampered payload accepted'; exit 1; fi
grep -q 'checksum mismatch' "$WORK/tamper.log"
[ "$(cat "$PREFIX/lighter.pid")" = "$original_pid" ]
stage "$FUTURE"
echo 'PASS: tampered kernel rejected before stopping VM'
if "$L" upgrade > "$WORK/refused.log" 2>&1; then echo 'FAIL: running upgrade accepted without --restart'; exit 1; fi
grep -q -- '--restart' "$WORK/refused.log"
[ "$(cat "$PREFIX/lighter.pid")" = "$original_pid" ]
"$L" upgrade --restart
[ "$(basename "$(readlink "$PREFIX/current")")" = "$future" ]
[ "$("${D[@]}" inspect -f '{{.State.Running}}' upgrade-container)" = true ]
[ "$("${D[@]}" run --rm -v upgrade-data:/data alpine:3.21 cat /data/value)" = survived ]
echo 'PASS: explicit running upgrade preserved container and volume'
stage "$BROKEN"
if "$L" upgrade --restart > "$WORK/rollback.log" 2>&1; then echo 'FAIL: nonbooting release was accepted'; exit 1; fi
grep -q 'restored previous release' "$WORK/rollback.log" || { cat "$WORK/rollback.log"; exit 1; }
[ "$(basename "$(readlink "$PREFIX/current")")" = "$future" ]
[ ! -e "$PREFIX/upgrade.json" ]
[ "$("${D[@]}" inspect -f '{{.State.Running}}' upgrade-container)" = true ]
[ "$("${D[@]}" run --rm -v upgrade-data:/data alpine:3.21 cat /data/value)" = survived ]
echo 'PASS: failed boot rolled back and restored running container and volume'
"$L" status
"$L" stop
