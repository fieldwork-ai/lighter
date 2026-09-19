#!/usr/bin/env bash
# Build the lighter-mps wheels: one per Python the containers are likely to
# run, each against the PyTorch release they are for. The wheels land in
# guest/out/wheels and the root filesystem carries them at
# /usr/lib/lighter/wheels, where `pip install lighter-mps` finds them offline
# (PIP_FIND_LINKS from the CDI device).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/guest/out/wheels"
TORCH="${LIGHTER_TORCH_VERSION:-2.14.0}"
PYTHONS="${LIGHTER_WHEEL_PYTHONS:-3.11 3.12 3.13}"
mkdir -p "$OUT"
for py in $PYTHONS; do
	echo "==> lighter-mps for Python $py, torch $TORCH"
	docker run --rm \
		-v "$ROOT/guest/torch-mps:/src:ro" -v "$OUT:/out" \
		-v "lighter-pip-cache:/root/.cache/pip" \
		"python:$py-slim" sh -c "
			set -e
			apt-get update -qq >/dev/null && apt-get install -y -qq --no-install-recommends g++ >/dev/null
			pip install -q setuptools wheel
			pip install -q --index-url https://download.pytorch.org/whl/cpu 'torch==$TORCH'
			cp -r /src /build && cd /build && rm -rf lighter_mps/*.so build
			python setup.py -q bdist_wheel --dist-dir /out
		" 2>&1 | grep -v "^\s*$" | tail -3
done
ls -la "$OUT"
