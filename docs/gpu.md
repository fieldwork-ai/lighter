# The GPU, the Neural Engine and PyTorch in containers

Three devices, each a CDI device name on `docker run`, all on by default and all costing nothing until a container uses them (`lighter config --gpu off`, `--ane off`, `--mps off` to remove them).

## Vulkan: `--device lighter.dev/gpu=all`

The guest has a virtio-gpu render node. Nothing else: no display, no cursor, one capset, Venus, which is Vulkan serialised over the virtqueue and decoded on the host by virglrenderer's Venus renderer on top of MoltenVK, on the Mac's own GPU. A container needs Mesa's Venus driver, which the distributions package (`mesa-vulkan-virtio` on Alpine, part of `mesa-vulkan-drivers` on Debian, Ubuntu and Fedora), plus the Vulkan loader:

```bash
docker run --rm --device lighter.dev/gpu=all alpine:edge sh -c \
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

PyTorch is not on this list: on Linux it has no Vulkan backend, so a container's `torch` stays on the CPU (the `lighter.dev/mps` device is the answer for PyTorch, below).

**How it is built.** The renderer is statically linked; `host/gpu/build.sh` builds virglrenderer (Venus only, render server as a thread) against the MoltenVK release archive into `host/out`, and the VMM's `build.rs` links what it finds there. Two patches are carried: the guest kernel places host-visible blobs on 16 KiB boundaries (Apple silicon's page; stock Mesa then works unchanged, `guest/kernel/patches/0031`), and virglrenderer gets an eventfd where macOS has none (`host/gpu/patches/0001`), without which its fence thread never runs and every Vulkan wait hangs.

**What it costs.** A blob the guest maps is a Metal buffer placed into guest-physical space by the hypervisor, in an 8 GiB aperture of address space above RAM; it belongs to the renderer and is never counted as guest memory. The renderer initialises when the driver probes, at boot: about 10 MB and two threads on an idle machine, the guest kernel's DRM about 3 ms of boot.

## The Neural Engine: `--device lighter.dev/ane=all`

A container's ONNX Runtime loads lighter's plugin execution provider, `/usr/lib/lighter/liblighter_ane_ep.so`, which claims the model's graph, serialises it and sends it to the host, where ONNX Runtime's CoreML provider runs it, on the Neural Engine where it can. The provider links no libc, so the one file loads in any image.

```python
import onnxruntime as ort
ort.register_execution_provider_library("lighter", "/usr/lib/lighter/liblighter_ane_ep.so")
devices = [d for d in ort.get_ep_devices() if d.ep_name == "LighterANE"]
options = ort.SessionOptions(); options.add_provider_for_devices(devices, {})
session = ort.InferenceSession("model.onnx", options)
```

The model crosses at the first run, when its input shapes are known: the Neural Engine takes only bound shapes, so the provider binds the model's inputs to that run's dims (and rebinds if they change). Nodes with subgraphs (`If`, `Loop`) stay on the container's CPU provider. ResNet-50 on an M1: 1.76 ms an inference on the Neural Engine, against 30 ms on the CPU and 7.9 ms on the GPU through CoreML.

**How it is built.** `host/ane/build.sh` builds ONNX Runtime with the CoreML provider as static archives into `host/out/ort`; `guest/ane-ep/build.sh` builds the provider. The host service is a loopback TCP port the container reaches through the streams (`lighter.ane=<port>` on the kernel command line, `LIGHTER_ANE` in the container); ONNX Runtime is loaded on the host at the first model, not at boot.

## PyTorch on the Mac's GPU: `--device lighter.dev/mps=all`

A container's PyTorch gets `torch.device("mps")`, and every operator it runs there is executed by the Mac's own PyTorch on its GPU. Nothing is bundled on the host: lighter finds a Python whose `torch` has MPS (the first `python3` on PATH that does, or `lighter config --torch-python <path>`) and starts a small server in it; without one, the device is simply absent and `lighter start` says so. In the container, `pip install lighter-mps` (the wheels are carried in the guest and found offline through `PIP_FIND_LINKS`) and `import lighter_mps`; then `model.to("mps")` works as it would on the Mac, training included.

```bash
docker run --rm --device lighter.dev/mps=all python:3.12-slim sh -c '
  pip install torch --index-url https://download.pytorch.org/whl/cpu && pip install lighter-mps &&
  python -c "import torch, lighter_mps; x = torch.randn(3, 3, device=\"mps\"); print((x @ x).device)"'
```

How it works: the extension occupies the MPS dispatch key, which a Linux build of PyTorch declares and never fills. A tensor on `mps` in the container holds no data, only a handle to a tensor on the Mac. One boxed fallback carries every operator across, with its arguments serialised by the schema's types; results come back as handles with the shape the container's tensor mirrors; autograd runs in the container and its backward operators cross the same way. Only copies to and from the CPU move bytes. The container's and the Mac's `torch` must share a major.minor version, and the server refuses a mismatch by name.

Eager mode is one round trip per operator over the streams. A 60-step training loop of a small MLP costs a few milliseconds a step; large models spend their time on the GPU and the round trips disappear into it.

## Gates

`make gate-m9` (Vulkan: vulkaninfo through the CDI device), `make gate-m10` (an ONNX model through the plugin provider, outputs checked against the CPU), `make gate-m11` (a container's PyTorch training a model and running a convolution on `mps`). All need `docker` on the Mac and pull an image; m11 needs a Python with torch and MPS on the Mac (`LIGHTER_GATE_TORCH_PYTHON`).
