#!/usr/bin/env bash
set -euo pipefail
export PATH="/opt/homebrew/bin:$PATH" HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1
cd "$HOME/lighter"
TAP="$(brew --repository fieldwork-ai/tap)"
git -C "$TAP" diff --quiet HEAD
FORMULA="$TAP/Formula/lighter.rb"
[ -f "$FORMULA" ] || FORMULA="$TAP/lighter.rb"
BACKUP="$(mktemp -t lighter-brew-formula)"
cp "$FORMULA" "$BACKUP"
cleanup() { cp "$BACKUP" "$FORMULA"; rm -f "$BACKUP"; }
trap cleanup EXIT
sed "s|https://github.com/fieldwork-ai/lighter/releases/download/v0.5.1/lighter-0.5.1-arm64.tar.gz|file://$PWD/dist/lighter-0.5.1-arm64.tar.gz|" .logs/051/lighter-brew-candidate.rb > "$FORMULA"
brew "${BREW_TEST_COMMAND:-upgrade}" fieldwork-ai/tap/lighter
brew postinstall fieldwork-ai/tap/lighter
brew postinstall fieldwork-ai/tap/lighter
L=/opt/homebrew/opt/lighter/bin/lighter
cmp dist/lighter-0.5.1-arm64 "$L"
"$L" status || test "$?" = 1
for args in 'update download' 'update auto-download on' 'upgrade'; do
 if "$L" $args > .logs/051/brew-refused.log 2>&1; then echo "FAIL: Brew accepted direct $args"; exit 1; fi
 grep -q 'Homebrew manages' .logs/051/brew-refused.log
done
"$L" update check
brew test fieldwork-ai/tap/lighter
codesign --verify --strict /opt/homebrew/opt/lighter/bin/lighter
codesign --verify --strict --deep /opt/homebrew/opt/lighter/share/lighter/lighter.app
spctl --assess --type execute /opt/homebrew/opt/lighter/share/lighter/lighter.app
python3 - <<'PY'
import hashlib,json,pathlib
root=pathlib.Path('/opt/homebrew/opt/lighter').resolve()
manifest=json.loads((root/'share/lighter/lighter.app/Contents/Resources/release.json').read_text())
assert manifest['version']=='0.5.1'
for name,wanted in manifest['files'].items():
 with (root/name).open('rb') as f: assert hashlib.file_digest(f,'sha256').hexdigest()==wanted,name
marker=json.loads((root/'share/lighter/installation.json').read_text())
assert marker['method']=='homebrew'
assert marker['prefix']==str(root.parent)
PY
echo 'PASS: Homebrew upgrade, repeated metadata registration, signed payload preservation, and direct-update refusal'
