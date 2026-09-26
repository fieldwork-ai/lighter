#!/bin/sh
# The media engine: the x264 case's twenty seconds of 1080p30 encoded in
# hardware. On the Mac, VideoToolbox; in a container, V4L2 (lighter's
# `lighter.sh/video` device). A runtime with neither has no hardware encoder,
# and the case says so rather than timing software.
set -eu
if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q " h264_videotoolbox "; then enc=h264_videotoolbox
elif ls /dev/video* >/dev/null 2>&1; then enc=h264_v4l2m2m
else echo "no hardware encoder" >&2; exit 64; fi
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=1920x1080:rate=30 -t 20 \
	-pix_fmt nv12 -c:v $enc -b:v 8M -f null -
