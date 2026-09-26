#!/bin/sh
# The corpus: the first 256 MiB of the llm case's model, the same bytes on
# every target, copied off the share first so the share's speed is not in it.
set -eu
head -c 268435456 "$WORK/models/qwen2.5-0.5b-instruct-q4_k_m.gguf" > "${TMPDIR:-/tmp}/zstd-corpus"
