# lighter 0.8.0: hardware video decode in containers

Status: built. Approved 2026-09-22, working end to end on the M5 the same night (gate m13), on the real stack (Frigate's own ffmpeg, the Reolink camera) after five fixes that only real streams found. This note is the record of the decisions and of those fixes; `gpu.md` is the user's documentation, `release-notes-0.8.0.md` the release. Experiments land as rows in `worklog.md`.

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

The synthetic gate passed on the first night; real streams failed five ways, in order, each fixed at its cause.

1. **Controls.** The Pi ffmpeg asks for `V4L2_CID_MIN_BUFFERS_FOR_CAPTURE` before anything else and refuses a decoder without it; the crate answered no controls at all. It now answers the two every stateful decoder has.
2. **Order.** VideoToolbox returns frames in decode order however it is asked (asynchronous decode with temporal processing was measured and changes nothing). Reordering by the guest's timestamps worked for Debian's ffmpeg and scrambled the Pi's, which stamps packets with decode-order sequence numbers. Order now comes from the stream: the picture order count in each slice header (`src/video/sps.rs`), with the guest's timestamp carried through untouched, as a hardware decoder does. The same session found the crate waiting for a LAST buffer after the initial resolution event, which no decoder sends, so the drain's EOS event was swallowed; fixed in the crate.
3. **The camera's SPS.** The Reolink E1 Pro writes a VUI that runs 8 bits past its end. ffmpeg decodes through it; VideoToolbox refuses the whole stream, native ffmpeg on the Mac included, which silently falls back to software. When VideoToolbox refuses, the device retries with the VUI taken out.
4. **A race in the driver.** `virtio_media_qbuf` counted a buffer as queued after the device's reply and outside `queues_lock`, while the dequeue event decrements under it. A decoder that completes within the command makes the two collide; the count crept up until poll never reported OUTPUT writable, and the Pi ffmpeg, which dequeues input only on POLLOUT, stalled every minute or so. Patch 0038 counts first, under the lock. It belongs upstream.
5. **A poll after the end.** The camera's main stream has no B-frames, so when it ends there is nothing left to reorder and the LAST buffer goes out inside the drain command. Debian's ffmpeg then polls once more for a frame; vb2 answers that poll readable once the last buffer is taken, so DQBUF can say EPIPE, and virtio-media did not, so ffmpeg waited forever. Patch 0040; m13 now decodes a clip without B-frames to its end.

## What it is worth

Decode in a stock Debian container on the M5, Debian's ffmpeg 5.1, sixteen vCPUs, the whole VMM's CPU time measured from outside, every frame compared with software decode by `framemd5` and identical on all three:

| Stream | Software, real time | Hardware, real time | Software, flat out | Hardware, flat out |
|---|---|---|---|---|
| 4K30, 25 Mbps, B-frames | 41% of a core | 8% | 467 fps, 14.2 ms a frame | 224 fps, 1.6 ms |
| 1080p60, 12 Mbps, B-frames | 29% | 6% | 1880 fps, 3.8 ms | 666 fps, 0.5 ms |
| The Reolink's main stream, 2880x1616 at 20 fps | 17% | 4% | 1535 fps, 5.3 ms | 488 fps, 0.65 ms |

Frigate, which is what this was for, on a recording of the camera replayed in a loop so runs compare (live, the scene moves the numbers by a third):

- Detecting on the sub stream (896x512 at 10 fps), as the home stack does: hardware and software decode both cost about 12.8% of a core for the whole VMM. The decode is a percent either way. What the stack paid for was elsewhere, and 0.8.0 fixes that too (next section).
- Detecting on the main stream at 1280x720: 32% of a core in software, 22% in hardware. Most of the rest is ffmpeg's bicubic downscale of 5 MP frames, 14% of a core for five a second; `fast_bilinear` does it for one.

## What Frigate actually paid for

With decode ruled out, the chase went through the VMM thread by thread (`proc_pidinfo` per named thread, 10 s windows; Instruments' Time Profiler for on-CPU stacks; `schedstat` in the guest for exact run time per process):

- **Idle polling was a third of it.** Each vCPU went idle about 300 times a second under Frigate; most polls timed out, and 0011's rule grew the window back after the short sleep that followed each. Polling was 60 to 80 ms of every second across four vCPUs. Patch 0039 judges polling by what it catches (in `architecture.md`); Frigate replayed on 16 vCPUs went from 20.7% of a core to 15.4%, and the cross-vCPU pipe round trip stays at 2.5 µs (8.5 without polling). Gate m12 then caught what the first version cost: llama.cpp's generation over Metal fell from 293 t/s to 267-273, because a token loop's reply arrives as an interrupt and a poll ended by one had been counted as buying nothing. While an accelerator stream is talking, it now counts; Frigate's detector traffic makes that cost about a point (15.4% against 14.5).
- **The host's queue poller the same, smaller:** a 200 µs window after every vsock kick, 1.3% of a core, now 0.6.
- **What is left is mostly Frigate.** The guest's threads ran 11.2% of a core against the vCPUs' 12.7% of host time, so entering and leaving the guest costs a point and a half; the Neural Engine server another point and a half (inference and its copies). Of the guest's time, `frigate.process` (motion detection in numpy) is 3.7 points, ffmpeg 2.3, Frigate's other Python processes 3.
- **Tried and not kept:** timer slack for the whole guest (1 and 4 ms: no change, Frigate's wakeups are events, not timers); parking containers on fewer vCPUs (one to two points on sixteen vCPUs, inside the noise, not worth a control loop that would sit in front of every burst).

A wakeup costs the Mac about 30 µs (a timer-driven loop in the guest, against 5 natively): two thirds in `hv_trap`, a fifth in Hypervisor.framework parking and waking the vCPU thread, and the rest its vGIC emulation, which traps the interrupt acknowledge and end-of-interrupt registers. That is Apple's, and it is why the remaining lever is fewer wakeups rather than cheaper ones.

## Not done

HEVC (VideoToolbox has it; the crate's decoder and the driver are codec-agnostic, so it is a format and a parameter-set path), encode, and zero copy.
