#!/usr/bin/env bash
# Private tools for comparable native/container measurements. No global installs.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TOOLS="$ROOT/.logs/050/tools/native"
ARCHIVE="$ROOT/.logs/050/tools/node-v24.18.0-darwin-arm64.tar.gz"
mkdir -p "$(dirname "$ARCHIVE")"
if [ ! -f "$ARCHIVE" ]; then
	curl -fL https://nodejs.org/dist/v24.18.0/node-v24.18.0-darwin-arm64.tar.gz -o "$ARCHIVE.part"
	mv "$ARCHIVE.part" "$ARCHIVE"
fi
[ "$(shasum -a 256 "$ARCHIVE" | awk '{print $1}')" = e1a97e14c99c803e96c7339403282ea05a499c32f8d83defe9ef5ec66f979ed1 ]
if [ ! -x "$TOOLS/bin/node" ]; then
	mkdir -p "$TOOLS"
	tar -xzf "$ARCHIVE" --strip-components=1 -C "$TOOLS"
fi
export PATH="$TOOLS/bin:$PATH"
if [ ! -x "$TOOLS/bin/pnpm" ] || [ ! -x "$TOOLS/bin/yarn" ] || [ "$(npm --version)" != 11.16.0 ] || [ "$(pnpm --version 2>/dev/null || true)" != 10.28.0 ] || [ "$(yarn --version 2>/dev/null || true)" != 1.22.22 ]; then
	npm --prefix "$TOOLS" --cache "$TOOLS/install-cache" install -g --force --no-audit --no-fund npm@11.16.0 pnpm@10.28.0 yarn@1.22.22
fi
python3 - "$ROOT/benchmarks/toolchain.json" "$TOOLS/versions.json" <<'PY'
import json, pathlib, subprocess, sys
expected = json.loads(pathlib.Path(sys.argv[1]).read_text())
actual = {name: subprocess.check_output([name, '--version'], text=True).strip()
          for name in ['node', 'npm', 'pnpm', 'yarn']}
assert all(actual[name] == expected[name] for name in actual), actual
pathlib.Path(sys.argv[2]).write_text(json.dumps(actual, indent=2) + '\n')
print(json.dumps(actual))
PY
