# The image every containerized benchmark target runs in.
#
# The package managers are pinned to the versions the native target has, so a
# comparison against macOS measures the filesystem rather than two different
# installers; `report.py` prints both sets of versions.
#
# `--force` because the node image ships its own `yarn` shim in
# /usr/local/bin and npm refuses to replace a file it did not create. Pinning
# our own over it is the point.
FROM node:24-alpine

RUN apk add --no-cache ripgrep git tar iperf3 \
	&& npm install -g --force --no-audit --no-fund pnpm@10.28.0 yarn@1.22.22

WORKDIR /work
