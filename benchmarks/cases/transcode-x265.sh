#!/bin/sh
# The same clip decoded and encoded with x265 (preset fast), 8 threads.
set -eu
ffmpeg -hide_banner -loglevel error -i "$WORK/media/bbb-1080p30-10s.mp4" -an \
	-c:v libx265 -preset fast -x265-params log-level=error:pools=8 -f null -
