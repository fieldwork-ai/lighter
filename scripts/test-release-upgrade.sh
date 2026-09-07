#!/usr/bin/env bash
# Real, notarized archive migration under an isolated HOME and PATH target.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OLD="${1:?0.4.1 archive}" NEW="${2:?0.4.2 archive}" BOOTSTRAP="${3:?signed 0.4.2 bootstrap}"
WORK="$(mktemp -d /tmp/lighter-migrate.XXXXXX)"
export HOME="$WORK/u"
export DOCKER_CONFIG="$HOME/.docker"
unset LIGHTER_HOME LIGHTER_GUEST_DIR
PREFIX="$HOME/.lighter"
L="$PREFIX/bin/lighter"
mkdir -p "$PREFIX" "$WORK/path"
cleanup() {
  result=$?
  if [ "$result" -ne 0 ]; then
    [ ! -f "$WORK/install.log" ] || tail -25 "$WORK/install.log" >&2
    [ ! -f "$PREFIX/machine.log" ] || tail -35 "$PREFIX/machine.log" >&2
  fi
  [ ! -x "$L" ] || "$L" stop >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT
[ "$(shasum -a 256 "$OLD" | awk '{print $1}')" = 2f14375ef2ea1e065de856de39cb00c84489ff208e350333d31d5717861e3a9b ]
tar -xzf "$OLD" --strip-components=1 -C "$PREFIX"
cat > "$PREFIX/config.json" <<'JSON'
{"cpus":2,"memory_mib":2048,"disk_gib":64,"shares":[],"publish":"localhost"}
JSON
cp "$PREFIX/config.json" "$WORK/config-before.json"
"$L" start --timeout 120
D=(docker -H "unix://$PREFIX/docker.sock")
"${D[@]}" run --rm -v migration-data:/data alpine:3.21 sh -c 'echo preserved > /data/value'
"${D[@]}" run -d --name migration-container --restart always alpine:3.21 sleep 3600 >/dev/null
original_pid="$(cat "$PREFIX/lighter.pid")"
if "$BOOTSTRAP" install-archive --archive "$NEW" --prefix "$PREFIX" > "$WORK/refused.log" 2>&1; then
  echo 'FAIL: upgrade did not require --restart' >&2; exit 1
fi
grep -q -- '--restart' "$WORK/refused.log" || { cat "$WORK/refused.log" >&2; exit 1; }
[ "$(cat "$PREFIX/lighter.pid")" = "$original_pid" ]
# Execute the real installer, redirecting only the system PATH target.
sed "s|/usr/local/bin|$WORK/path|g" "$ROOT/scripts/install.sh" > "$WORK/install.sh"
LIGHTER_VERSION=0.4.2 LIGHTER_TARBALL_URL="file://$NEW" LIGHTER_BOOTSTRAP_URL="file://$BOOTSTRAP" GITHUB_TOKEN= bash "$WORK/install.sh" --restart > "$WORK/install.log" 2>&1
"$L" status
[ "$("${D[@]}" run --rm -v migration-data:/data alpine:3.21 cat /data/value)" = preserved ]
[ "$("${D[@]}" inspect -f '{{.State.Running}}' migration-container)" = true ]
cmp "$WORK/config-before.json" "$PREFIX/config.json"
[ "$(readlink "$PREFIX/current")" = releases/0.4.2 ]
[ ! -e "$PREFIX/upgrade.json" ]
python3 - "$PREFIX" <<'PY'
import pathlib,json,sys
root=pathlib.Path(sys.argv[1])
identity=json.loads((root/'machine.identity').read_text())
assert identity['release_version']=='0.4.2', identity
assert identity['kernel_version']=='6.18.49', identity
assert json.loads((root/'share/lighter/installation.json').read_text())['method']=='script'
assert any((root/'releases').glob('legacy-*'))
PY
"$L" stop
echo 'release upgrade: real installer verified; 0.4.1 -> 0.4.2 preserved running container, named volume and configuration; explicit restart enforced'
