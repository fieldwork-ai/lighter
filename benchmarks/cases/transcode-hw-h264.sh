#!/bin/sh
# The media engine: the clip decoded and encoded in hardware, H.264 to H.264
# at 8 Mbit/s. On the Mac, VideoToolbox; in a container, V4L2 both ways
# (lighter's `lighter.sh/video` device). A runtime with neither has no
# hardware transcoder, and the case says so rather than timing software.
set -eu
in="$WORK/media/bbb-1080p30-10s.mp4"
if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q " h264_videotoolbox "; then
	ffmpeg -hide_banner -loglevel error -hwaccel videotoolbox -i "$in" -an -c:v h264_videotoolbox -b:v 8M -f null -
elif ls /dev/video* >/dev/null 2>&1; then
	ffmpeg -hide_banner -loglevel error -c:v h264_v4l2m2m -i "$in" -an -c:v h264_v4l2m2m -b:v 8M -f null -
else echo "no hardware encoder" >&2; exit 64; fi
