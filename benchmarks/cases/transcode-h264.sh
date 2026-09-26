#!/bin/sh
# A transcode the fastest way the runtime can do it: Big Buck Bunny, ten
# seconds of 1080p30 H.264 at 24.5 Mbit/s (CC-BY, Blender Foundation), decoded
# and encoded again as H.264. On the media engine where there is one
# (VideoToolbox on the Mac, frames kept on the engine; V4L2 both ways in a
# container with `lighter.sh/video`), otherwise x264 (preset medium) on 8
# threads. A clip rather than a generated pattern: ffmpeg's pattern generator
# runs on one thread and was the limit.
set -eu
in="$WORK/media/bbb-1080p30-10s.mp4"
if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q " h264_videotoolbox "; then
	ffmpeg -hide_banner -loglevel error -hwaccel videotoolbox -hwaccel_output_format videotoolbox_vld \
		-i "$in" -an -c:v h264_videotoolbox -b:v 8M -f null -
elif ls /dev/video* >/dev/null 2>&1; then
	ffmpeg -hide_banner -loglevel error -c:v h264_v4l2m2m -i "$in" -an -c:v h264_v4l2m2m -b:v 8M -f null -
else
	ffmpeg -hide_banner -loglevel error -i "$in" -an -c:v libx264 -preset medium -threads 8 -f null -
fi
