# lighter 0.9.0

Containers decode HEVC and VP9 and encode H.264 and HEVC on the Mac's media engine, alongside the H.264 decode 0.8.0 brought.

## Decode: H.264, HEVC and VP9

`/dev/video0` now takes HEVC, in 8 and 10 bits, and VP9, in profiles 0 and 2, where VideoToolbox decodes it in hardware (on Apple silicon it does, once its supplemental decoder is registered, which the device does). ffmpeg's `hevc_v4l2m2m` and `vp9_v4l2m2m` and GStreamer's `v4l2h265dec` and `v4l2vp9dec` find them with no change. Eight-bit streams decode to frames identical to software decode's, every one; ten-bit streams come out as NV12 by default (the top eight bits, since ffmpeg's V4L2 wrapper has no ten-bit format) within 55 to 58 dB of software's own conversion, and as P010 for a client that asks. HEVC is ordered by its picture order count, as H.264 is, with random-access-skipped pictures dropped after a CRA the way a decoder must; VP9 shows frames in the order it decodes them and needs no reordering. The decoder is now codec-blind (`src/video/decoder.rs`), with each codec's syntax behind one trait (`h264.rs`, `hevc.rs`, `vp9.rs`).

There is no AV1: no Linux client drives a V4L2 AV1 decoder, and the M1 has no AV1 hardware. The device neither lists nor accepts it.

## Encode: H.264 and HEVC

`/dev/video1` is a V4L2 stateful encoder: NV12, YU12 or P010 in; H.264 (Baseline, Main, High) or HEVC (Main, and Main 10 from P010 or on request) out, at the bitrate, GOP and profile asked for, with forced keyframes, constant or variable bitrate and QP bounds, through the controls encoders are configured with. `docker run --device lighter.sh/video=all` gives a container both nodes.

Measured on the M1 (4 vCPUs, the whole VMM's CPU, `benchmarks/video.sh`), the share of a core it costs to keep up at 30 fps:

| Encode | Software | V4L2 | Native VideoToolbox |
|---|---|---|---|
| H.264 1080p, 8 Mbps | 78% (libx264 veryfast) | 22% | 9% |
| HEVC 1080p, 6 Mbps | cannot keep up (libx265 ultrafast, 28 fps on four cores) | 21% | 9% |
| HEVC 4K, 20 Mbps | cannot keep up (7 fps) | 42% | 26% |

| Decode | Software | V4L2 | Native VideoToolbox |
|---|---|---|---|
| H.264 4K30 | 69% | 16% | 12% |
| HEVC Main 10 4K30 | 100% | 30% | 15% |
| VP9 1080p30 | 44% | 9% | 6% |

A 4K H.264 stream transcoded to 1080p HEVC entirely in hardware keeps up at 94% of a core against 91% natively (most of it ffmpeg's scale from 4K, on the CPU either way); software manages 28 fps. Flat out, the encoder does 171 fps at 1080p and 51 at 4K. What encoding costs over native is the container's own work, reading raw frames and copying them into the encoder's buffers, not the device's.

It works with what already speaks V4L2: ffmpeg 5.1 and 7.1 (`h264_v4l2m2m`, `hevc_v4l2m2m`), GStreamer 1.26 (`v4l2h264enc`, `v4l2h265enc`), Jellyfin's V4L2 hardware acceleration (tested transcoding H.264, HEVC 10-bit 4K, VP9 and AV1 sources), go2rtc's `#hardware=v4l2m2m`, and Frigate, which decodes an H.265 camera through `hevc_v4l2m2m` and can take a go2rtc restream encoded on the same machine.

Clients each had a requirement a device must meet, and the encoder meets them:

- ffmpeg asks for its parameter sets in a separate buffer, then muxes that buffer as a packet with no picture, which an MP4 muxer takes as a timestamp going backwards; the device offers only headers joined to the keyframe, which ffmpeg accepts. Every keyframe carries them, so a stream can be joined anywhere.
- ffmpeg copies frames in at its own strides whatever the device's `bytesperline` says (unpadded from some filters, rounded to 16 from its buffer pools, chroma rounded on its own for planar YUV); the device reads the layout from the bytes written.
- ffmpeg 7.1 ends a drain the first moment it finds no CAPTURE buffer queued, and asks for four, which its muxer holds long enough to lose the last frames of every stream; the device grants at least twelve. It also holds back frames while encoded output waits for a buffer, as hardware that encodes into one must.
- GStreamer keeps each frame's buffer until its output comes back, so with B-frames, which VideoToolbox looks ahead up to a dozen frames for, the device asks for enough OUTPUT buffers to cover that.
- B-frames are off unless asked for (ffmpeg's wrapper refuses to run with them); asked, VideoToolbox places up to three between references.

VideoToolbox returns encoded frames on its own thread; it rings a doorbell, and a thread of the device's takes the transport lock and delivers them, as the GPU's fences do.

## Also

- Guest patch 0041: `VIDIOC_G_CTRL` and `VIDIOC_S_CTRL` through virtio-media failed with EINVAL, because the V4L2 core builds them as an extended control on its stack with its size uninitialized and the driver sent that as a payload pointer. GStreamer sets an encoder's profile that way; `v4l2-ctl` reads the decoder's minimum buffers that way.
- Gate `m14` encodes through the device with ffmpeg 5.1 and 7.1 and GStreamer and checks every frame, the bitrate and keyframes asked for, odd sizes and planar input, a transcode entirely on the media engine, B-frames and Main 10, and the CPU against libx264 and libx265. Gate `m13` now decodes HEVC and VP9 as well.
- `benchmarks/video.sh` measures decode and encode against software in the same container and against VideoToolbox called natively on the host.
- The video aperture is 2 GiB, half for each device, and costs nothing until used. The CDI spec lists every `/dev/video` node.
- `lighter config --disk` now applies to a disk that already exists: the data disk grows to the configured size at the next start, and the guest grows the filesystem to fill it, online. Disks never shrink, and the CLI says so when asked to. Before, the size was only read when the image was first created, and the setting silently did nothing.
- `lighter restart` could leave the machine stopped: the stop returned before launchd had reaped the old process, and launchd ignored the start. It now restarts the job regardless.
- Linux remains **6.18.52**, with guest patches 0036 to 0041; the data epoch remains **1**.
