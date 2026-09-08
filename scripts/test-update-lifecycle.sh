#!/usr/bin/env bash
# Hardware test: an ad-hoc signed, isolated managed layout. Does not exercise
# release trust (the exact notarized archive smoke does that separately).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
VERSION="$(target/release/lighter --version | awk '{print $2}')"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Invalid test CLI version" >&2; exit 1; }
WORK="$(mktemp -d /tmp/lighter-update.XXXXXX)"
PREFIX="$WORK/install"
RELEASE="$PREFIX/releases/$VERSION"
export HOME="$WORK/user"
unset LIGHTER_HOME LIGHTER_GUEST_DIR
MACHINE_HOME="$HOME/.lighter"
mkdir -p "$HOME" "$MACHINE_HOME" "$RELEASE/bin" "$RELEASE/share/lighter"
L="$PREFIX/bin/lighter"
cleanup() { result=$?; if [ "$result" -ne 0 ] && [ -f "$MACHINE_HOME/machine.log" ]; then cat "$MACHINE_HOME/machine.log" >&2; fi; [ ! -x "$L" ] || "$L" stop >/dev/null 2>&1 || true; [ ! -x "$L" ] || "$L" update auto-download off >/dev/null 2>&1 || true; python3 "$ROOT/scripts/records/unregister-test-bundles.py" "$WORK" || true; rm -rf "$WORK"; }
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
ln -s "releases/$VERSION" "$PREFIX/current"
ln -s ../current/bin/lighter "$L"
ln -s ../current/share/lighter "$PREFIX/share/lighter"
python3 - "$PREFIX" "$VERSION" <<'PY'
import hashlib,json,pathlib,os,sys
p=pathlib.Path(sys.argv[1]).resolve(); root=p/'releases'/sys.argv[2]
parts=[int(n) for n in sys.argv[2].split('.')]; parts[-1]+=1
future='.'.join(map(str,parts))
identity=hashlib.sha256(str(p).encode()).hexdigest()[:24]
(root/'share/lighter/installation.json').write_text(json.dumps(dict(schema=1,method='script',id=identity,prefix=str(p))))
pathlib.Path(str(pathlib.Path(os.environ['HOME'],'.lighter')),'config.json').write_text(json.dumps(dict(cpus=2,memory_mib=2048,disk_gib=64,shares=[],publish='localhost')))
state=pathlib.Path(os.environ['HOME'],'Library/Application Support/lighter/updates',identity);state.mkdir(parents=True)
(state/'state.json').write_text(json.dumps(dict(automatic=False,available=future,downloaded=future,checked=__import__('time').time().__int__(),attempted=__import__('time').time().__int__())))
PY
"$L" start --timeout 120
D=(docker -H "unix://$MACHINE_HOME/docker.sock")
"${D[@]}" run --rm -v update-persistence:/data alpine:3.21 sh -c 'echo persistent > /data/value'
"${D[@]}" run -d --name update-survivor --restart always alpine:3.21 sleep 3600 >/dev/null
"$L" restart
[ "$(readlink "$PREFIX/current")" = "releases/$VERSION" ]
[ "$("${D[@]}" run --rm -v update-persistence:/data alpine:3.21 cat /data/value)" = persistent ]
[ "$("${D[@]}" inspect -f '{{.State.Running}}' update-survivor)" = true ]
"$L" status
python3 - "$VERSION" <<'PY'
import json,os,pathlib,sys
identity=json.loads(pathlib.Path(str(pathlib.Path(os.environ['HOME'],'.lighter')),'machine.identity').read_text())
assert identity['release_version']==sys.argv[1],identity
assert identity['kernel_version']=='6.18.49',identity
PY
"$L" stop
[ "$(readlink "$PREFIX/current")" = "releases/$VERSION" ]
"$L" update auto-download on
"$L" update poll
[ "$(readlink "$PREFIX/current")" = "releases/$VERSION" ]
"$L" update auto-download off
[ -z "$(find "$HOME/Library/LaunchAgents" -name 'dev.lighter.updates.*.plist' -print)" ]
echo 'update lifecycle: pending release never activated; restart preserved volume/container/config; running versions verified'
