# The image every containerized benchmark target runs in.
#
# The package managers are pinned to the versions the native target has, so a
# comparison against macOS measures the filesystem rather than two different
# installers; `report.py` prints both sets of versions.
#
# `--force` because the node image ships its own `yarn` shim in
# /usr/local/bin and npm refuses to replace a file it did not create. Pinning
# our own over it is the point.
FROM node:24.18.0-alpine@sha256:a0b9bf06e4e6193cf7a0f58816cc935ff8c2a908f81e6f1a95432d679c54fbfd

RUN apk add --no-cache ripgrep git tar iperf3 \
	&& npm install -g --force --no-audit --no-fund npm@11.16.0 pnpm@10.28.0 yarn@1.22.22

WORKDIR /work
