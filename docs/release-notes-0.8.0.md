# lighter 0.8.0

Containers decode H.264 on the Mac's media engine, and a home server that
wakes a few hundred times a second stops paying a third of its CPU for
the privilege.

## Hardware video decode in containers

`docker run --device lighter.sh/video=all` gives a container `/dev/video0`,
a V4L2 stateful H.264 decoder of the kind a Raspberry Pi has, backed by
VideoToolbox. Whatever already speaks V4L2 uses it unchanged: ffmpeg's
`h264_v4l2m2m`, GStreamer's `v4l2h264dec`, and the Raspberry Pi build of
ffmpeg that Frigate ships. Nothing is installed in the container and
nothing is bundled on the host; the device is on by default
(`lighter config --video off`).

Decoded in real time in a stock Debian container on the M5, every frame
identical to software decode's:

| Stream | Software | `h264_v4l2m2m` |
|---|---|---|
| 4K at 30 fps, 25 Mbps | 41% of a core | 8% |
| 1080p at 60 fps, 12 Mbps | 29% | 6% |
| A camera's 2880x1616 at 20 fps | 17% | 4% |

Flat out the media engine decodes that 4K stream at 224 fps for 1.6 ms of
the Mac's CPU a frame; software on sixteen vCPUs does 467 fps for 14.2.

For Frigate, `ffmpeg.hwaccel_args: -c:v h264_v4l2m2m` on the camera (not
`preset-rpi-64-h264`, which names a second video stream a camera does not
have and so decodes in software). Detecting on a 5 MP main stream, the
whole machine goes from 32% of a core to 22%. Detecting on a sub stream,
the decode was a percent either way.

How: the guest's driver is virtio-media, the v9 series from the kernel list;
the host device is the `virtio-media` crate's stateful decoder over
VideoToolbox, in the VMM. Five things only real streams found, each fixed
where it lived (`docs/video-decode-2026-09-22.md`): the Pi ffmpeg asks for
two controls first; VideoToolbox hands frames back in decode order, and the
device now orders them by the stream's picture order count, not the
timestamps a client may set to anything; the Reolink camera's SPS has a
VUI that VideoToolbox refuses, and native ffmpeg on the Mac too, so the
device retries without it; the driver counted queued buffers outside its
lock and a decoder as fast as this one lost the race every minute or so
(guest patch 0038); and it did not report the queue readable after the
last buffer, as vb2 does, so Debian's ffmpeg hung at the end of a stream
with no B-frames (0040). Gate `m13` decodes through the device in a
container and compares every frame.

## Idle polling only while it pays

Frigate on one camera, detecting on the Neural Engine, cost 20.7% of a core
on a 16-vCPU guest; it costs 14.5%. A third had been idle polling: each
vCPU went idle about 300 times a second, most polls timed out, and the
short sleep after each grew the window back. Guest patch 0039 grows the
window only on a poll another CPU ended, halves it on a timeout, and every
16 polls weighs the spinning against the wakeups it caught, backing off
from 10 ms to 250 ms while that is more than 20 µs a catch. A thread
pool's hand-offs, which polling is for, keep it: the cross-vCPU round trip
stays at 2.5 µs (8.5 without polling). The host's queue poller does the
same with its windows, and `idle.polled` reports how each poll ended.

What Frigate costs now is mostly Frigate: its motion detection and Python
processes are most of the guest's time, and entering and leaving the
guest a point and a half.

## Also

- Kicks a driver sends before it is ready are serviced when it is, not
  dropped with a warning (virtio-media fills its event queue in probe).
- The media device's commands and events can be traced
  (`LIGHTER_LOG=lighter_vmm::virtio::media=trace`).
- Linux remains **6.18.52**, with guest patches 0036 to 0040; the data
  epoch remains **1**.
