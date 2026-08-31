# The image every containerized benchmark target runs in.
#
# Pinned by digest rather than by tag, because a benchmark whose toolchain moves
# between runs measures the toolchain. The host comparison uses whatever the Mac
# has; `run.sh` records both versions in the report so the two can be compared
# honestly rather than silently.
FROM node:24-alpine

RUN apk add --no-cache ripgrep git tar

WORKDIR /work
