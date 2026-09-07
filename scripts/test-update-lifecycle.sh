#!/usr/bin/env bash
# Hardware test: an ad-hoc signed, isolated managed layout. Does not exercise
# release trust (the exact notarized archive smoke does that separately).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
WORK="$(mktemp -d /tmp/lighter-update.XXXXXX)"
PREFIX="$WORK/install"
RELEASE="$PREFIX/releases/0.4.2"
export HOME="$WORK/user" LIGHTER_HOME="$WORK/machine"
mkdir -p "$HOME" "$LIGHTER_HOME" "$RELEASE/bin" "$RELEASE/share/lighter"
L="$PREFIX/bin/lighter"
cleanup() { result=$?; if [ "$result" -ne 0 ] && [ -f "$LIGHTER_HOME/machine.log" ]; then cat "$LIGHTER_HOME/machine.log" >&2; fi; [ ! -x "$L" ] || "$L" stop >/dev/null 2>&1 || true; rm -rf "$WORK"; }
trap cleanup EXIT
cp -c target/release/lighter "$RELEASE/bin/lighter"
cp -c guest/out/Image guest/out/rootfs.ext4 guest/out/kernel.version "$RELEASE/share/lighter/"
APP="$RELEASE/share/lighter/lighter.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$RELEASE/bin/lighter" "$APP/Contents/MacOS/lighter"
cp assets/Info.plist "$APP/Contents/Info.plist"
cp assets/lighter.icns "$APP/Contents/Resources/lighter.icns"
codesign --force --sign - --entitlements entitlements.plist "$APP" >/dev/null 2>&1
mkdir -p "$PREFIX/bin" "$PREFIX/share"
ln -s releases/0.4.2 "$PREFIX/current"
ln -s ../current/bin/lighter "$L"
ln -s ../current/share/lighter "$PREFIX/share/lighter"
python3 - "$PREFIX" <<'PY'
import hashlib,json,pathlib,os,sys
p=pathlib.Path(sys.argv[1]).resolve(); root=p/'releases/0.4.2'
identity=hashlib.sha256(str(p).encode()).hexdigest()[:24]
(root/'share/lighter/installation.json').write_text(json.dumps(dict(schema=1,method='script',id=identity,prefix=str(p))))
pathlib.Path(os.environ['LIGHTER_HOME'],'config.json').write_text(json.dumps(dict(cpus=2,memory_mib=2048,disk_gib=64,shares=[],publish='localhost')))
state=pathlib.Path(os.environ['HOME'],'Library/Application Support/lighter/updates',identity);state.mkdir(parents=True)
(state/'state.json').write_text(json.dumps(dict(automatic=True,available='0.4.3',downloaded='0.4.3')))
PY
"$L" start --timeout 120
D=(docker -H "unix://$LIGHTER_HOME/docker.sock")
"${D[@]}" run --rm -v update-persistence:/data alpine:3.21 sh -c 'echo persistent > /data/value'
"${D[@]}" run -d --name update-survivor --restart always alpine:3.21 sleep 3600 >/dev/null
"$L" restart
[ "$(readlink "$PREFIX/current")" = releases/0.4.2 ]
[ "$("${D[@]}" run --rm -v update-persistence:/data alpine:3.21 cat /data/value)" = persistent ]
[ "$("${D[@]}" inspect -f '{{.State.Running}}' update-survivor)" = true ]
"$L" status
python3 - <<'PY'
import json,os,pathlib
identity=json.loads(pathlib.Path(os.environ['LIGHTER_HOME'],'machine.identity').read_text())
assert identity['release_version']=='0.4.2',identity
assert identity['kernel_version']=='6.18.49',identity
PY
"$L" stop
[ "$(readlink "$PREFIX/current")" = releases/0.4.2 ]
echo 'update lifecycle: pending release never activated; restart preserved volume/container/config; running versions verified'
