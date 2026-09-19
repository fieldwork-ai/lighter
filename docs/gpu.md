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

What remains between the container and native is the request loop per token: eight RPC messages, each a wake of the guest and of the server thread, and the logits back. Three things keep the loop tight, all scoped to the time a model is answering. The server thread runs at the interactive QoS class, without which macOS parks it on an efficiency core while the vCPUs hold the performance ones (a native client to the same server went from 94 to 107 tokens a second on the M1 for that alone). While a container has a stream open to the server the vCPUs are held at the interactive class too (`qos::Boost`, ended when the stream closes), because a vCPU woken late on a busy Mac made the container's number swing between 63 and 80 from run to run. And for the same stream's life the guest agent widens the window the guest kernel polls before it sleeps a vCPU, from the resting 50–200 µs to 2–5 ms (`guest/agent/src/accelerator.rs`), so the client's threads stay awake between messages the way a native client's do: 64 tokens a second became 95 to 99 on the M1, against 107 for a native client of the same server. Nothing else is boosted or kept awake: an ordinary container competes with the Mac's windows no harder than any process, and an idle machine still costs nothing. Both hold for exactly as long as the stream is open, so a container that keeps its connection to a device open keeps them; that is the device in use, which is what the container asked for. Larger models spend proportionally more time in the kernels and less in the loop.

whisper.cpp is the second worked example (`examples/whisper-metal`): a Wyoming speech-to-text service for Home Assistant with the model on the Mac's GPU, small.en transcribing an 11 s clip in 1.30 s against 5.80 s on the container's CPU and 1.36 s native, base.en in 0.45 s against 1.46 and 0.38, on an M1 with caches warm.

**Clients.** The listener is lighter's, not ggml's. ggml's own server accepts on a socket it bound, serves one client at a time, and leaves its loop on the first `accept` error, which macOS raises for a queued client that resets before it is taken; a killed benchmark left the device dead for the machine's life. lighter accepts, and hands each descriptor to a carried ggml entry point (`host/metal/patches/0001`) on a thread of its own with ggml backends of its own, so a resident whisper.cpp service and an on-demand llama.cpp share the GPU, and a client that resets is a log line.

**Loading.** A model's weights cross the stream once per process, at a few hundred megabytes a second. ggml hashes every weight over 10 MiB and asks the server first; lighter gives its server a cache directory (`ggml-cache` under the lighter home), so a weight the server has seen before is read from disk instead. A model whose tensors are that size, which is every layer of a 7B model and few of whisper base's, loads the second time at the cost of its small tensors. The directory grows with the models used and is safe to delete.

**What reaches the servers.** The three services (Neural Engine, ggml on Metal, PyTorch) are loopback TCP ports on the Mac, and the CDI device sets environment variables that name them: today any container can open a stream to them through the host gateway whether or not it asked for the device, and each parses what it is sent inside the lighter process (ggml's own note is that its protocol is not hardened). A container is therefore closer to lighter's address space than it was in 0.6.0, where loopback services were the Mac's own. The gate is the guest agent's, which carries every container stream: a stream to an accelerator port is refused unless dockerd records a CDI request for that device on the container the stream comes from, found by its address; a container that did not ask for `--device lighter.sh/metal` gets a reset from the port. What remains is that a container which did ask reaches a parser in lighter's process, which is the device's nature; the ports never leave loopback.

**Versions.** ggml's RPC protocol is versioned and checked at connect: the container's ggml must speak the version lighter was built with. `host/metal/build.sh` pins the llama.cpp commit and writes the protocol version to `host/out/ggml/rpc-proto-version`; a mismatch fails with ggml's own message at the first request. ggml notes the protocol is not hardened, which is why the server binds loopback only and is reachable solely through lighter's streams.

**How it is built.** The renderer is statically linked; `host/gpu/build.sh` builds virglrenderer (Venus only, render server as a thread) against the MoltenVK release archive into `host/out`, and the VMM's `build.rs` links what it finds there. Two patches are carried: the guest kernel places host-visible blobs on 16 KiB boundaries (Apple silicon's page; stock Mesa then works unchanged, `guest/kernel/patches/0031`), and virglrenderer gets an eventfd where macOS has none (`host/gpu/patches/0001`), without which its fence thread never runs and every Vulkan wait hangs.

**What it costs.** A blob the guest maps is a Metal buffer placed into guest-physical space by the hypervisor, in an 8 GiB aperture of address space above RAM; it belongs to the renderer and is never counted as guest memory. The renderer initialises when the driver probes, at boot: about 10 MB and two threads on an idle machine, the guest kernel's DRM about 3 ms of boot.

## The Neural Engine: `--device lighter.sh/ane=all`

A container's ONNX Runtime loads lighter's plugin execution provider, `/usr/lib/lighter/liblighter_ane_ep.so`, which claims the model's graph, serialises it and sends it to the host, where ONNX Runtime's CoreML provider runs it, on the Neural Engine where it can. The provider links no libc, so the one file loads in any image.

The worked example is Frigate (`examples/frigate-ane`): a forty-line detector plugin and a derived image with a newer ONNX Runtime, and YOLO11n runs at 7.6–7.9 ms a frame on the Neural Engine with the detector process at under one percent CPU, against 15.1 ms and 36% for Frigate's own CPU detector on the same clip, on an M1.

```python
import onnxruntime as ort
ort.register_execution_provider_library("lighter", "/usr/lib/lighter/liblighter_ane_ep.so")
devices = [d for d in ort.get_ep_devices() if d.ep_name == "LighterANE"]
options = ort.SessionOptions(); options.add_provider_for_devices(devices, {})
session = ort.InferenceSession("model.onnx", options)
```

The model crosses at the first run, when its input shapes are known: the Neural Engine takes only bound shapes, so the provider binds the model's inputs to that run's dims (and rebinds if they change). Nodes with subgraphs (`If`, `Loop`) stay on the container's CPU provider. ResNet-50 on an M1: 1.76 ms an inference on the Neural Engine, against 30 ms on the CPU and 7.9 ms on the GPU through CoreML.

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

## Gates

`make gate-m9` (Vulkan: vulkaninfo through the CDI device), `make gate-m10` (an ONNX model through the plugin provider, outputs checked against the CPU), `make gate-m11` (a container's PyTorch training a model and running a convolution on `mps`), `make gate-m12` (llama-bench in a container over RPC to Metal, at least 80% of native generation when a native `llama-bench` is given). All need `docker` on the Mac and pull an image; m11 needs a Python with torch and MPS on the Mac (`LIGHTER_GATE_TORCH_PYTHON`); m12 needs an image with llama.cpp built with `GGML_RPC` (`LIGHTER_GATE_LLAMA_IMAGE_TAR`).
