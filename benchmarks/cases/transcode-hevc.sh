#!/bin/sh
# The same clip transcoded to HEVC the fastest way the runtime can: on the
# media engine where there is one (as in transcode-h264), otherwise x265
# (preset fast) with a pool of 8 threads, all at 8 Mbit/s.
set -eu
# TRANSCODE_OUT keeps the result for transcode-check.sh; timed runs discard it.
out="${TRANSCODE_OUT:--f null -}"
in="$WORK/media/bbb-1080p30-10s.mp4"
if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q " hevc_videotoolbox "; then
	ffmpeg -hide_banner -loglevel error -hwaccel videotoolbox -hwaccel_output_format videotoolbox_vld \
		-i "$in" -an -c:v hevc_videotoolbox -b:v 8M $out
elif ls /dev/video* >/dev/null 2>&1; then
	ffmpeg -hide_banner -loglevel error -c:v h264_v4l2m2m -i "$in" -an -c:v hevc_v4l2m2m -b:v 8M $out
else
	ffmpeg -hide_banner -loglevel error -i "$in" -an \
		-c:v libx265 -preset fast -b:v 8M -x265-params log-level=error:pools=8 $out
fi
