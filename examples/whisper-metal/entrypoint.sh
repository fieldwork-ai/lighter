#!/bin/sh
# whisper-server on the RPC device named by the lighter.sh/metal device, and
# the Wyoming shim in front of it. MODEL is a ggml whisper model file.
set -e
MODEL=${MODEL:-/models/ggml-base.en.bin}
whisper-server -m "$MODEL" --host 127.0.0.1 --port 8080 -t "${THREADS:-4}" ${WHISPER_ARGS:-} &
for i in $(seq 1 120); do curl -sf -o /dev/null http://127.0.0.1:8080/ && break; sleep 1; done
exec python3 /usr/local/bin/wyoming-whispercpp --uri tcp://0.0.0.0:10300 --server http://127.0.0.1:8080 --language "${LANGUAGE:-en}"
