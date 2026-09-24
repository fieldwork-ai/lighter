# The GPU, the Neural Engine, PyTorch and ggml in containers

Four devices, each a CDI device name on `docker run`, all on by default and all costing nothing until a container uses them (`lighter config --gpu off`, `--ane off`, `--mps off`, `--metal off` to remove them).

## Vulkan: `--device lighter.sh/gpu=all`

The guest has a virtio-gpu render node. Nothing else: no display, no cursor, one capset, Venus, which is Vulkan serialised over the virtqueue and decoded on the host by virglrenderer's Venus renderer on top of MoltenVK, on the Mac's own GPU. A container needs Mesa's Venus driver, which the distributions package (`mesa-vulkan-virtio` on Alpine, part of `mesa-vulkan-drivers` on Debian, Ubuntu and Fedora), plus the Vulkan loader:

```bash
docker run --rm --device lighter.sh/gpu=all alpine:edge sh -c \
  'apk add mesa-vulkan-virtio vulkan-loader vulkan-tools && vulkaninfo --summary'
#   deviceName = Virtio-GPU Venus (Apple M1)
```

What runs on it is whatever speaks Vulkan: llama.cpp's Vulkan backend, whisper.cpp, stable-diffusion.cpp, ncnn, ONNX Runtime's WebGPU provider. Measured on an M1 with four vCPUs, Qwen2.5-0.5B (Q4_K_M), `llama-bench -p 128 -n 32`:

| where | prompt, t/s | generation, t/s |
|---|---:|---:|
| container, Vulkan | 1028 | 45 |
| container, CPU | 329 | 21 |
| native macOS, Metal | 1923 | 99 |
| native macOS, CPU | 229 | 79 |

PyTorch is not on this list: on Linux it has no Vulkan backend, so a container's `torch` stays on the CPU (the `lighter.sh/mps` device is the answer for PyTorch, below). And the generation number is a ceiling every Venus stack shares: ggml's Vulkan shaders go through MoltenVK, which exposes neither cooperative matrices nor integer dot products, where ggml's Metal backend has hand-written kernels for both. For llama.cpp and the rest of the ggml family, `lighter.sh/metal` is the faster door.

## ggml on the Mac's GPU: `--device lighter.sh/metal=all`

llama.cpp, whisper.cpp, stable-diffusion.cpp and anything else on ggml can hand their tensors to a ggml RPC server; lighter runs that server in-process, on the Mac's Metal backend with ggml's own kernels. The weights cross once at load and a few kilobytes of activations cross per token. The container's build needs `GGML_RPC=ON`; the device sets `LIGHTER_METAL` and `LLAMA_ARG_RPC` to the server, so `llama-server`, `llama-cli` and the other tools that read their arguments from the environment use it without a flag, and `llama-bench` takes `--rpc "$LIGHTER_METAL"`.

```bash
docker run --rm --device lighter.sh/metal=all -v models:/models llama-cpp-rpc \
  llama-bench -m /models/qwen2.5-0.5b-instruct-q4_k_m.gguf --rpc "$LIGHTER_METAL" -ngl 99
```

Same model, same M1, `llama-bench -p 128 -n 32`:

| where | prompt, t/s | generation, t/s |
|---|---:|---:|
| container, `lighter.sh/metal` | 1911 | 95 |
| container, `lighter.sh/gpu` (Vulkan) | 1028 | 45 |
| container, CPU | 329 | 21 |
| native macOS, Metal | 1953 | 110 |

What remains between the container and native is the request loop per token: eight RPC messages, each a wake of the guest and of the server thread, and the logits back. Three things keep the loop tight, all scoped to the time a model is answering. The server thread runs at the interactive QoS class, without which macOS parks it on an efficiency core while the vCPUs hold the performance ones (a native client to the same server went from 94 to 107 tokens a second on the M1 for that alone). While a container has a stream open to the server the vCPUs are held at the interactive class too (`qos::Boost`, ended when the stream closes), because a vCPU woken late on a busy Mac made the container's number swing between 63 and 80 from run to run. And for the same stream's life the guest agent raises the cap on the window the guest kernel polls before it sleeps a vCPU, from the resting 200 µs to 5 ms (`guest/agent/src/accelerator.rs`), so the client's threads stay awake between messages the way a native client's do: 64 tokens a second became 95 to 99 on the M1, against 107 for a native client of the same server. Only the cap is raised, and the kernel decides when the window is worth having (kernel patches 0011 and 0032): what decides is whether a message moved on that container's stream within the last millisecond, a message being a page or less, counted where sockmap moves them on the two sockets the agent marks. If one did, a model is talking and the window grows to its cap whatever woke the CPU, the tick or a thread's hand-off in the middle of a token being no reason to give it up; a tensor's bulk does not count, since a wakeup saved is worth nothing to a transfer, and a millisecond is an RPC gap's length, which a model's compute and a frame's transfer both exceed. If nothing moved, a window past the resting 200 µs comes back down to it, and at the resting size the guest's own timer, whose time the kernel knew at idle entry, halves the window rather than growing it. While a model is answering, then, the window sits at the cap; with the stream idle it falls to its floor within a few rounds. That is what lets a resident client hold its connection for its lifetime, Frigate's detector or a whisper server, and cost the Mac nothing between requests: held open for the stream's life on a kernel that grew the window on any wakeup, the M1's home server spent a full host core spinning its four vCPUs for an idle guest; with this rule it reads 21–39% with Frigate detecting at 4.4 frames a second, against 16–28% with no window at all, and the token loop keeps its 91–97 tokens a second. Nothing else is boosted or kept awake: an ordinary container competes with the Mac's windows no harder than any process, and an idle machine still costs nothing. Larger models spend proportionally more time in the kernels and less in the loop.

whisper.cpp is the second worked example (`examples/whisper-metal`): a Wyoming speech-to-text service for Home Assistant with the model on the Mac's GPU, small.en transcribing an 11 s clip in 1.30 s against 5.80 s on the container's CPU and 1.36 s native, base.en in 0.45 s against 1.46 and 0.38, on an M1 with caches warm.

**Clients.** The listener is lighter's, not ggml's. ggml's own server accepts on a socket it bound, serves one client at a time, and leaves its loop on the first `accept` error, which macOS raises for a queued client that resets before it is taken; a killed benchmark left the device dead for the machine's life. lighter accepts, and hands each descriptor to a carried ggml entry point (`host/metal/patches/0001`) on a thread of its own with ggml backends of its own, so a resident whisper.cpp service and an on-demand llama.cpp share the GPU, and a client that resets is a log line.

**Loading.** A model's weights cross the stream once per process, at a few hundred megabytes a second. ggml hashes every weight over 10 MiB and asks the server first; lighter gives its server a cache directory (`ggml-cache` under the lighter home), so a weight the server has seen before is read from disk instead. A model whose tensors are that size, which is every layer of a 7B model and few of whisper base's, loads the second time at the cost of its small tensors. The directory grows with the models used and is safe to delete.

**Where the variables are.** The device's variables (`LIGHTER_METAL`, `LIGHTER_ANE`, `LIGHTER_MPS`) are injected into the container's process at creation, the way CDI works in Docker, and `docker exec` does not see them: a process started that way builds its environment from the image and `-e`, not from the created container's spec, so a client run through `docker exec` reports the variable unset (the ONNX provider's message is "LIGHTER_ANE is not set"). Run the client as the container's command, or pass the value with `-e` to `docker exec`.

**What reaches the servers.** The three services (Neural Engine, ggml on Metal, PyTorch) are loopback TCP ports on the Mac, and the CDI device sets environment variables that name them: today any container can open a stream to them through the host gateway whether or not it asked for the device, and each parses what it is sent inside the lighter process (ggml's own note is that its protocol is not hardened). A container is therefore closer to lighter's address space than it was in 0.6.0, where loopback services were the Mac's own. The gate is the guest agent's, which carries every container stream: a stream to an accelerator port is refused unless dockerd records a CDI request for that device on the container the stream comes from, found by its address; a container that did not ask for `--device lighter.sh/metal` gets a reset from the port. What remains is that a container which did ask reaches a parser in lighter's process, which is the device's nature; the ports never leave loopback.

**Versions.** ggml's RPC protocol is versioned and checked at connect: the container's ggml must speak the version lighter was built with. `host/metal/build.sh` pins the llama.cpp commit and writes the protocol version to `host/out/ggml/rpc-proto-version`; a mismatch fails with ggml's own message at the first request. ggml notes the protocol is not hardened, which is why the server binds loopback only and is reachable solely through lighter's streams.

**How it is built.** The renderer is statically linked; `host/gpu/build.sh` builds virglrenderer (Venus only, render server as a thread) against the MoltenVK release archive into `host/out`, and the VMM's `build.rs` links what it finds there. Two patches are carried: the guest kernel places host-visible blobs on 16 KiB boundaries (Apple silicon's page; stock Mesa then works unchanged, `guest/kernel/patches/0031`), and virglrenderer gets an eventfd where macOS has none (`host/gpu/patches/0001`), without which its fence thread never runs and every Vulkan wait hangs.

**What it costs.** A blob the guest maps is a Metal buffer placed into guest-physical space by the hypervisor, in an 8 GiB aperture of address space above RAM; it belongs to the renderer and is never counted as guest memory. The renderer initialises when the driver probes, at boot: about 10 MB and two threads on an idle machine, the guest kernel's DRM about 3 ms of boot.

## The Neural Engine: `--device lighter.sh/ane=all`

A container's ONNX Runtime loads lighter's library, `/usr/lib/lighter/liblighter_ane_ep.so`, which sends the model to the host, where ONNX Runtime's CoreML provider runs it. The library links no libc, so the one file loads in any image, and ONNX Runtime can load it two ways:

- **As a custom operator, from ONNX Runtime 1.16.** `lighter_ane_wrap` turns a model into one node, `lighter.ane:Model`, carrying the original, and `register_custom_ops_library` registers the op; the whole model runs on the host. This is the route for an image whose ONNX Runtime predates plugin providers (Frigate ships 1.22). The library asks for 1.16's C API on this route, and a test reads the source to hold every call inside that table.
- **As a plugin execution provider, from ONNX Runtime 1.23**, the first with plugin providers. It claims the graph's nodes and leaves any with subgraphs (`If`, `Loop`) to the container's CPU provider. It presents the Neural Engine as a CPU-type device, since naming a device of its own needs 1.25.

Both reach the same host service and cost the same: YOLO11n at 320 px ran at 2.1 ms either way on ONNX Runtime 1.23 in a container on an M5, against 6.3 ms on the container's CPU.

**Where a model runs.** The device is ONNX to CoreML, and CoreML covers the Mac's GPU and CPU as well as the Neural Engine, so the host loads a model three ways and the first run decides: the `NeuralNetwork` format with every compute unit, which is what reaches the Neural Engine; `MLProgram` with the CPU and GPU, for a graph the Neural Engine will not take whole; and ONNX Runtime's own CPU as the floor. Each is timed on the run's real inputs after a warm-up, the fastest is kept, and the choice is a line in the machine log with the three numbers. `LIGHTER_ANE_UNITS=ane|gpu|cpu` pins one. CoreML's compiled models are cached in `coreml-cache` under the lighter home, so a model, or a shape of it, compiles once. On an M1, against the container's own CPU provider: ResNet-50 29 → 2.2 ms (the Neural Engine), MiniLM-L6 at 128 tokens 10.8 → 6.3 ms (the GPU; the Neural Engine took the graph in pieces at 23 ms), Piper's voice model no faster (its graph crashes CoreML's Neural Engine path, fails on the GPU path, and runs on the host's CPU, which the log says). The first load of a model pays the compiles, 4 s for ResNet-50 and 21 s for MiniLM; after that the cache.

**A process of its own.** Apple's frameworks crash on some graphs: Piper's voice model took a machine down through a segfault in a CoreML convolution kernel. So the service runs as a child of `lighter start`, the same binary with a hidden `ane-host` subcommand, on a loopback port the parent chose and the guest was told. When it dies it is started again on that port; the container's request fails once and its ONNX Runtime falls back to the CPU provider for that run. The runner writes a marker for the candidate it is about to run for the first time and removes it after, so a model that crashed one candidate skips it at the next load. The PyTorch device is arranged the same way; the ggml server stays in-process, which `gpu.md`'s Metal section says.

The worked example is Frigate (`examples/frigate-ane`): Frigate's own image with one detector plugin added, which runs the model through the custom op on the ONNX Runtime Frigate ships. `benchmarks/ane-models.sh` runs every model type that detector takes through Frigate's own pipeline, exported with Frigate's documented recipes, and holds each confident detection to the container's CPU. On an M1, milliseconds a detection including Frigate's pre- and post-processing, the custom op on ONNX Runtime 1.22 (the plugin provider on 1.23 is within 0.3 ms of it on every model):

| Model | Container CPU | lighter | Where the host ran it |
|---|---:|---:|---|
| YOLOv9-t, 320 | 12.4 | 3.3 | Neural Engine |
| YOLOv9-s, 640 | 110.2 | 13.0 | Neural Engine |
| YOLO11n, 320 | 10.9 | 4.3 | Neural Engine |
| YOLOX-tiny, 416 | 31.3 | 12.1 | Neural Engine |
| YOLO-NAS-S, 320 | 32.0 | 6.6 | Neural Engine |
| RF-DETR Nano, 320 | 82.3 | 38.3 | GPU |
| D-FINE-S, 640 | 119.9 | 120.4 | the host's CPU: CoreML refuses it, so no faster |

Every confident detection matched the CPU's on both routes (`benchmarks/results/ane-models-m1.txt`).

**When CoreML fails a run.** A CoreML partition can fail on one input and run the next: YOLO-NAS carries its own NMS, and on a frame with nothing in it the NMS leaves an empty tensor its CoreML partition will not take. The host keeps the CPU candidate loaded when another unit wins and answers any run the winner fails from it (logged once), and a candidate that fails on the first run's input races again on the next runs', so a detector warmed up on a blank frame still reaches the Neural Engine.

```python
import ctypes, onnxruntime as ort
LIB = "/usr/lib/lighter/liblighter_ane_ep.so"

# Any ONNX Runtime from 1.16: the custom op.
lib = ctypes.CDLL(LIB)
lib.lighter_ane_wrap.argtypes = [ctypes.c_char_p, ctypes.c_size_t,
    ctypes.POINTER(ctypes.POINTER(ctypes.c_uint8)), ctypes.POINTER(ctypes.c_size_t)]
lib.lighter_ane_free.argtypes = [ctypes.POINTER(ctypes.c_uint8), ctypes.c_size_t]
model = open("model.onnx", "rb").read()
out, n = ctypes.POINTER(ctypes.c_uint8)(), ctypes.c_size_t()
assert lib.lighter_ane_wrap(model, len(model), ctypes.byref(out), ctypes.byref(n)) == 0  # -2: external weights
wrapped = ctypes.string_at(out, n.value); lib.lighter_ane_free(out, n.value)
options = ort.SessionOptions(); options.register_custom_ops_library(LIB)
session = ort.InferenceSession(wrapped, options, providers=["CPUExecutionProvider"])

# ONNX Runtime 1.23 on: the plugin execution provider.
ort.register_execution_provider_library("lighter", LIB)
devices = [d for d in ort.get_ep_devices() if d.ep_name == "LighterANE"]
options = ort.SessionOptions(); options.add_provider_for_devices(devices, {})
session = ort.InferenceSession("model.onnx", options)
```

The model crosses at the first run, when its input shapes are known: the Neural Engine takes only bound shapes, so the library binds the model's inputs to that run's dims (and rebinds if they change). On the custom-op route a model whose weights are kept in files beside it (ONNX external data) cannot cross, since the host cannot see those files, and `lighter_ane_wrap` refuses it with -2; the plugin provider sends the weights ONNX Runtime has already read, so it takes such a model. ResNet-50 on an M1: 1.76 ms an inference on the Neural Engine, against 30 ms on the CPU and 7.9 ms on the GPU through CoreML.

**How it is built.** `host/ane/build.sh` builds ONNX Runtime with the CoreML provider as static archives into `host/out/ort`; `guest/ane-ep/build.sh` builds the provider. The host service is a loopback TCP port the container reaches through the streams (`lighter.ane=<port>` on the kernel command line, `LIGHTER_ANE` in the container); ONNX Runtime is loaded on the host at the first model, not at boot.

## PyTorch on the Mac's GPU: `--device lighter.sh/mps=all`

A container's PyTorch gets `torch.device("mps")`, and every operator it runs there is executed by the Mac's own PyTorch on its GPU. Nothing is bundled on the host: lighter finds a Python whose `torch` has MPS (the first `python3` on PATH that does, or `lighter config --torch-python <path>`) and starts a small server in it; without one, the device is simply absent and `lighter start` says so. In the container, `pip install lighter-mps` (the wheels are carried in the guest and found offline through `PIP_FIND_LINKS`) and `import lighter_mps`; then `model.to("mps")` works as it would on the Mac, training included.

```bash
docker run --rm --device lighter.sh/mps=all python:3.12-slim sh -c '
  pip install torch --index-url https://download.pytorch.org/whl/cpu && pip install lighter-mps &&
  python -c "import torch, lighter_mps; x = torch.randn(3, 3, device=\"mps\"); print((x @ x).device)"'
```

How it works: the extension occupies the MPS dispatch key, which a Linux build of PyTorch declares and never fills. A tensor on `mps` in the container holds no data, only a handle to a tensor on the Mac. One boxed fallback carries every operator across, with its arguments serialised by the schema's types; results come back as handles with the shape the container's tensor mirrors; autograd runs in the container and its backward operators cross the same way. Only copies to and from the CPU move bytes. The container's and the Mac's `torch` must share a major.minor version, and the server refuses a mismatch by name.

Eager mode is one round trip per operator over the streams. A 60-step training loop of a small MLP costs a few milliseconds a step; large models spend their time on the GPU and the round trips disappear into it.

## Video on the media engine: `--device lighter.sh/video=all`

A container gets `/dev/video0`, a V4L2 memory-to-memory decoder (H.264, HEVC in 8 and 10 bits, and VP9 where the Mac decodes it in hardware), and `/dev/video1`, the matching encoder (H.264, and HEVC Main and Main 10), of the kind a Raspberry Pi or a phone has. Whatever already speaks V4L2 decodes and encodes on the Mac's media engine through VideoToolbox: ffmpeg's `h264_v4l2m2m`, `hevc_v4l2m2m` and `vp9_v4l2m2m`, GStreamer's `v4l2h264dec`/`v4l2h265dec`/`v4l2vp9dec` and `v4l2h264enc`/`v4l2h265enc`, Jellyfin's V4L2 hardware acceleration, go2rtc's `#hardware=v4l2m2m`, and Frigate's own Raspberry Pi build of ffmpeg. Nothing is installed in the container and nothing is bundled on the host.

```bash
docker run --rm --device lighter.sh/video=all debian:bookworm-slim sh -c '
  apt-get update -qq && apt-get install -y -qq ffmpeg >/dev/null &&
  ffmpeg -c:v h264_v4l2m2m -i input.mkv -c:v hevc_v4l2m2m -b:v 6M output.mp4'
```

Measured on the M1 (4 vCPUs, Debian's ffmpeg 5.1, the whole VMM's CPU time against ffmpeg's own for VideoToolbox called natively on the Mac; `benchmarks/video.sh`, results in `benchmarks/results/video-m1.txt`). Real time is the share of a core it costs to keep up with the stream; flat out is frames a second and CPU a frame. The VMM idles at 2.7% of a core, included.

| Decode | Software, real time | V4L2, real time | Native VideoToolbox, real time | V4L2, flat out |
|---|---|---|---|---|
| H.264 4K30, 25 Mbps | 69% | 16% | 12% | 79 fps, 5.1 ms a frame |
| H.264 1080p60, 12 Mbps | 50% | 13% | 11% | 252 fps, 1.7 ms |
| HEVC Main 10 4K30, 20 Mbps | 100% | 30% | 15% | 83 fps, 6.0 ms |
| VP9 1080p30, 6 Mbps | 44% | 9% | 6% | 231 fps, 1.8 ms |

| Encode, 30 fps | Software, real time | V4L2, real time | Native VideoToolbox, real time | V4L2, flat out |
|---|---|---|---|---|
| H.264 1080p, 8 Mbps (libx264 veryfast) | 78% | 22% | 9% | 171 fps, 3.9 ms |
| HEVC 1080p, 6 Mbps (libx265 ultrafast) | cannot keep up (28 fps on four cores) | 21% | 9% | 170 fps, 4.0 ms |
| HEVC 4K, 20 Mbps (libx265 ultrafast) | cannot keep up (7 fps) | 42% | 26% | 51 fps, 9.2 ms |

A 4K H.264 stream transcoded to 1080p HEVC entirely on the media engine keeps up at 94% of a core, against 91% natively; software manages 28 fps. Most of that is ffmpeg's scale from 4K, which runs on the CPU either way.

Decode through V4L2 costs a few points of a core more than native; flat out it outruns native ffmpeg, which downloads each frame in a second step. Ten-bit streams cost more: the device reads VideoToolbox's sixteen-bit frame and keeps the top eight bits of each sample itself, because VideoToolbox's own conversion to eight bits smooths chroma across sharp colour edges (up to 70 levels from software's, however the stream is tagged). Encoding costs about twice native in real time, and a profile of the VMM puts that in the guest, not the device: the container's ffmpeg reading raw frames and copying each into the encoder's buffer, work a native encode does in one process.

**How it works.** The guest's driver is virtio-media (the v9 series on the kernel list, carried as patches 0036 and 0037, with 0038 fixing a race in its buffer accounting that a decoder answering within the command, as this one does, hits every minute or so, and 0040 reporting the capture queue readable after its LAST buffer as vb2 does, without which Debian's ffmpeg hangs at the end of a stream with nothing left to reorder, and 0041, a fix to the V4L2 core sent upstream, zeroing the extended control it builds for `VIDIOC_G_CTRL`/`VIDIOC_S_CTRL`, whose stack garbage the driver took as a payload pointer; GStreamer sets an encoder's profile that way), which relays V4L2 to the host rather than implementing a decoder. The host device is the `virtio-media` crate's stateful decoder (`third_party/`) behind lighter's adapters (`crates/lighter-vmm/src/virtio/media.rs`), and the decoder is VideoToolbox (`src/video/`): each access unit is split at its start codes, the parameter sets (or VP9's keyframe header) become a format description, the slices go to VideoToolbox length-prefixed, and the frame comes back as NV12 into a buffer the guest has mapped. VideoToolbox returns frames in decode order (asynchronous decode with temporal processing was tried and changes nothing), so the device reorders them itself, by the picture order count in each slice header and never by the guest's timestamps, which a client may set to anything: the Pi ffmpeg Frigate ships stamps packets with decode-order sequence numbers. It holds back as many frames as the stream's SPS declares (`src/video/h264.rs`, `src/video/hevc.rs`), or learns the depth from the stream when it declares nothing, which costs a camera without B-frames no latency.

**Cameras.** VideoToolbox is stricter about the SPS than ffmpeg: it refuses the Reolink E1 Pro's, whose VUI runs 8 bits past its end, and native ffmpeg on the Mac falls back to software over it. When it refuses, the device retries with the VUI taken out (colour and timing advice a decoder does not need, `h264::without_vui`) and logs that it did.

**Frigate.** Pass the decoder as `ffmpeg.hwaccel_args: -c:v h264_v4l2m2m` on the camera and give the container `lighter.sh/video=all`. Not `preset-rpi-64-h264`: it expands to `-c:v:1 h264_v4l2m2m`, which names the second video stream, and a camera has one, so it silently decodes in software.

What it saves depends on what Frigate decodes. Detecting on a camera's sub stream (896x512 at 10 fps) the decode is a percent of a core either way, and hardware decode is a wash. Detecting on the 5 MP main stream (2880x1616 at 20 fps, detect at 1280x720) it is most of the bill: the whole VMM went from 32% of a core to 22%. Most of what remains is Frigate's own downscale, which ffmpeg's default bicubic scaler does at 14% of a core for five frames a second; ffmpeg's `fast_bilinear` does it for about one, which Frigate does not expose.

**Encoding.** The encoder takes NV12, YU12 or P010 and gives H.264 (Baseline, Main, High) or HEVC (Main, or Main 10 from P010 input or on request through the profile control), at the bitrate, GOP and profile asked for, with forced keyframes, constant or variable bitrate, and QP bounds. Parameter sets travel with every keyframe, so a stream can be joined anywhere; the SEPARATE header mode ffmpeg asks for is refused, because ffmpeg then muxes the headers as a packet with no picture. B-frames are off unless asked for (ffmpeg's V4L2 wrapper refuses to run with them); asked for, VideoToolbox picks up to three between references. Frames go to VideoToolbox as they are queued and come back on its thread, which rings a doorbell the device's thread answers (`src/video/encoder.rs`, `src/video/encoder_device.rs`). ffmpeg 5.1 and 7.1 copy frames in at their own strides whatever `bytesperline` says, so the device reads the layout from the bytes written; ffmpeg 7.1 ends a drain the first moment it finds no CAPTURE buffer queued, so the device grants at least twelve, whatever is asked. ffmpeg's V4L2 encoders do not write global headers, so mux to MP4, MPEG-TS or a raw stream rather than Matroska, as on a Pi.

**Timestamps.** A container over RTSP, MP4 or Matroska carries a timestamp on every frame, and the device returns each frame with its own. A bare `.h264` file has none: ffmpeg then sends zero for every frame and drops all but the first as duplicates, which is ffmpeg's behaviour with any V4L2 decoder. Wrap it (`-f matroska`) first.

**Limits.** Up to 8192 pixels a side. Ten-bit streams decode to NV12 by default (the top eight bits, since ffmpeg's V4L2 wrapper has no P010) and to P010 for a client that asks. No AV1: no Linux client drives a V4L2 AV1 decoder, and the M1 has no AV1 hardware. GStreamer 1.26 cannot feed P010 to a single-plane device, so ten-bit encoding from GStreamer takes eight-bit input with `extra-controls=encode,hevc_profile=2`. Each frame is copied once each way, between VideoToolbox's buffer and the guest's; zero copy waits on the driver's DMA-BUF support upstream. The buffers live in their own address-space aperture beside the GPU's and cost nothing until used. `lighter config --video off` removes both devices.

## Gates

`make gate-m9` (Vulkan: vulkaninfo through the CDI device), `make gate-m10` (an ONNX model through the plugin provider, outputs checked against the CPU), `make gate-m11` (a container's PyTorch training a model and running a convolution on `mps`), `make gate-m13` (a container's ffmpeg decoding H.264, HEVC and VP9 through `/dev/video0`, every frame compared with software decode, and the Mac's CPU a frame measured against software), `make gate-m14` (ffmpeg 5.1 and 7.1 and GStreamer encoding H.264 and HEVC through `/dev/video1`: every frame, the bitrate and keyframes asked for, odd sizes, B-frames and Main 10, and the CPU against libx264 and libx265), `make gate-m12` (llama-bench in a container over RPC to Metal, at least 80% of native generation when a native `llama-bench` is given). All need `docker` on the Mac and pull an image; m11 needs a Python with torch and MPS on the Mac (`LIGHTER_GATE_TORCH_PYTHON`); m12 needs an image with llama.cpp built with `GGML_RPC` (`LIGHTER_GATE_LLAMA_IMAGE_TAR`).
