#!/bin/sh
# The same five seconds of 1080p30 in x265 (preset fast): HEVC in software.
set -eu
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=1920x1080:rate=30 -t 5 \
	-c:v libx265 -preset fast -x265-params log-level=error:pools=8 -f null -
