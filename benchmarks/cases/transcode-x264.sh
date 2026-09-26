#!/bin/sh
# A real transcode in software: Big Buck Bunny, ten seconds of 1080p30 H.264
# at 24.5 Mbit/s (CC-BY, Blender Foundation), decoded and encoded again with
# x264 (preset medium) on 8 threads. A clip rather than a generated pattern:
# ffmpeg's pattern generator runs on one thread and was the limit.
set -eu
ffmpeg -hide_banner -loglevel error -i "$WORK/media/bbb-1080p30-10s.mp4" -an \
	-c:v libx264 -preset medium -threads 8 -f null -
