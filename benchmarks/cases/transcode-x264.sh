#!/bin/sh
# Twenty seconds of 1080p30 test video encoded in software with x264 (preset
# medium, every core): a real CPU workload, no disk, no network. The media
# engine is not in it; lighter's hardware path is measured separately.
set -eu
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=1920x1080:rate=30 -t 20 \
	-c:v libx264 -preset medium -f null -
