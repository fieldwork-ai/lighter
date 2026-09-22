# lighter 0.8.0: hardware video decode in containers

Status: built. Approved 2026-09-22, working end to end on the M5 the same night (gate m13), on the real stack (Frigate's own ffmpeg, the Reolink camera) after four fixes that only the real stack found. This note is the record of the decisions and of those fixes; `gpu.md` is the user's documentation, `release-notes-0.8.0.md` the release. Experiments land as rows in `worklog.md`.

## What ships

`docker run --device lighter.sh/video=all` gives a container `/dev/video0`: a V4L2 stateful memory-to-memory H.264 decoder, the kind a Raspberry Pi or a phone has, backed by VideoToolbox on the Mac. Whatever already speaks V4L2 decodes on the media engine unchanged: ffmpeg's `h264_v4l2m2m`, GStreamer's `v4l2h264dec`, and the Raspberry Pi build of ffmpeg that Frigate ships. Nothing is installed in the container and nothing is bundled on the host.

## Decisions recorded

1. **virtio-media, not VAAPI and not virtio-video.** A VAAPI shim would be a library injected across every image's libc and libva, a copy per frame, and a second path to support; virtio-video is the older spec its own authors replaced. virtio-media relays V4L2 itself, so the guest needs a driver and nothing else. The driver is the v9 series from the kernel list (17 Sep 2026), carried as patches 0036 and 0037 on 6.18; the device id is already upstream.
2. **The device is in the VMM, like the GPU**, not a helper process like `ane-host`: VideoToolbox is a C API in a system framework, called over hand-written FFI (`src/video/vt_sys.rs`), with nothing to build or bundle.
3. **The host side is the `virtio-media` crate** (ChromiumOS, BSD-3) and `v4l2r`, vendored in `third_party/` because both build only on Linux as published. Their patches are listed in `third_party/README.md`; each is a line or two, and the device framework, the protocol and the stateful decoder state machine are theirs.
4. **NV12 out, one copy per frame**, from VideoToolbox's pixel buffer into POSIX shared memory the guest has mapped through its own aperture (`GuestLayout::video`, 1 GiB of address space, no memory until used). Zero copy waits on the driver's DMA-BUF support upstream.
5. **Synchronous decode under the transport lock**, the GPU's model: a 1080p frame is a millisecond or two of the media engine. It is also what exposed the driver race below; an asynchronous device would have hidden it.
6. **On by default** (`lighter config --video off`), delivered by CDI (`lighter.sh/video`), written by the guest's init.

## What only the real stack found

The synthetic gate passed on the first night; Frigate on the camera failed four ways, in order, each fixed at its cause.

1. **Controls.** The Pi ffmpeg asks for `V4L2_CID_MIN_BUFFERS_FOR_CAPTURE` before anything else and refuses a decoder without it; the crate answered no controls at all. It now answers the two every stateful decoder has.
2. **Order.** VideoToolbox returns frames in decode order however it is asked (asynchronous decode with temporal processing was measured and changes nothing). Reordering by the guest's timestamps worked for Debian's ffmpeg and scrambled the Pi's, which stamps packets with decode-order sequence numbers. Order now comes from the stream: the picture order count in each slice header (`src/video/sps.rs`), with the guest's timestamp carried through untouched, as a hardware decoder does. The same session found the crate waiting for a LAST buffer after the initial resolution event, which no decoder sends, so the drain's EOS event was swallowed; fixed in the crate.
3. **The camera's SPS.** The Reolink E1 Pro writes a VUI that runs 8 bits past its end. ffmpeg decodes through it; VideoToolbox refuses the whole stream, native ffmpeg on the Mac included, which silently falls back to software. When VideoToolbox refuses, the device retries with the VUI taken out.
4. **A race in the driver.** `virtio_media_qbuf` counted a buffer as queued after the device's reply and outside `queues_lock`, while the dequeue event decrements under it. A decoder that completes within the command makes the two collide; the count crept up until poll never reported OUTPUT writable, and the Pi ffmpeg, which dequeues input only on POLLOUT, stalled every minute or so. Patch 0038 counts first, under the lock. It belongs upstream.

## What it is worth

MEASUREMENTS

## Not done

HEVC (VideoToolbox has it; the crate's decoder and the driver are codec-agnostic, so it is a format and a parameter-set path), encode, and zero copy.
