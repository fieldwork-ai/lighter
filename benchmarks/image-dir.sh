#!/usr/bin/env bash
# Builds the benchmark image once and writes it as the directory
# `LIGHTER_BENCH_IMAGE_DIR` loads (`benchmarks/run.sh`): `<arch>.tar` and a
# `manifest.json` with each archive's hash and image ID, which the harness
# checks after loading. Every target then runs byte-identical images, and a
# runtime that cannot build them itself (Podman and socktainer have no
# BuildKit; buildx builds for them in a container of its own and never loads
# the result) still gets them.
#
#   benchmarks/image-dir.sh <out-dir> [docker context to build with] [arches]
set -euo pipefail
cd "$(dirname "$0")/.."
OUT="${1:?usage: benchmarks/image-dir.sh <out-dir> [context] [arches]}"
CTX="${2:-$(docker context show)}"
ARCHES="${3:-arm64}"
IMAGE="$(sed -n 's/^IMAGE="\(.*\)"$/\1/p' benchmarks/run.sh | head -1)"
mkdir -p "$OUT"
manifest="{}"
for arch in $ARCHES; do
	tag="$IMAGE"; [ "$arch" = arm64 ] || tag="$IMAGE-$arch"
	docker --context "$CTX" build -q --platform "linux/$arch" -t "$tag" benchmarks >/dev/null
	docker --context "$CTX" save -o "$OUT/$arch.tar" "$tag"
	sha="$(shasum -a 256 "$OUT/$arch.tar" | cut -d' ' -f1)"
	id="$(docker --context "$CTX" image inspect -f '{{.Id}}' "$tag")"
	manifest="$(python3 -c 'import json,sys; m=json.loads(sys.argv[1]); m[sys.argv[2]]={"archive_sha256":sys.argv[3],"image_id":sys.argv[4]}; print(json.dumps(m, indent=2))' "$manifest" "$arch" "$sha" "$id")"
	echo "$arch: $tag $id"
done
printf '%s\n' "$manifest" > "$OUT/manifest.json"
