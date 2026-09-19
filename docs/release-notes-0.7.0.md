# lighter 0.7.0

## The GPU, the Neural Engine, PyTorch and ggml, in containers

Four things no other Docker for macOS offers, each a device name on
`docker run`, each on by default and costing nothing until a container uses
it (`docs/gpu.md`).

**Vulkan** (`--device lighter.sh/gpu=all`). The guest has a virtio-gpu
render node speaking Venus, decoded on the Mac by virglrenderer over MoltenVK,
on the Mac's own GPU. A stock image with Mesa's Venus driver
(`mesa-vulkan-virtio` on Alpine, in `mesa-vulkan-drivers` on Debian) sees
`Virtio-GPU Venus (Apple M1)`, and llama.cpp's Vulkan backend runs on it: on an
M1 with four vCPUs, Qwen2.5-0.5B does 1028 tokens/s of prompt processing and
45 tokens/s of generation in a container, against 329 and 21 on the
container's CPU and 1923 and 99 on native Metal. Blobs the guest maps are
Metal buffers placed in an 8 GiB aperture above RAM by the hypervisor, never
counted as guest memory. Two patches carried: the guest kernel places
host-visible blobs on 16 KiB boundaries so stock Mesa works, and virglrenderer
gets an eventfd where macOS has none, without which every Vulkan wait hangs.

**The Neural Engine** (`--device lighter.sh/ane=all`). A container's ONNX
Runtime loads lighter's plugin execution provider,
`/usr/lib/lighter/liblighter_ane_ep.so`, which links no libc so the one file
loads in any image. It claims the model, serialises it and sends it to the
Mac, where ONNX Runtime's CoreML provider runs it on the Neural Engine
(ResNet-50: 1.76 ms an inference, against 30 ms on the CPU). The model
crosses at the first run, when its shapes are known, since the Neural Engine
takes only bound shapes; nodes with subgraphs stay on the container's CPU.

**PyTorch on the Mac's GPU** (`--device lighter.sh/mps=all`). A container's
`torch` gets `torch.device("mps")`, and the Mac's own `torch` executes every
operator asked of it, on its GPU. `pip install lighter-mps` inside the
container (the wheels are carried in the guest) and `import lighter_mps`;
`model.to("mps")` then works as on the Mac, training included, autograd in the
container and the operators crossing one at a time. Nothing is bundled on the
host: lighter finds a Python whose `torch` has MPS (`lighter config
--torch-python`) and starts a small server in it; the container's and the
Mac's torch must share a major.minor.

**ggml on the Mac's GPU** (`--device lighter.sh/metal=all`). llama.cpp,
whisper.cpp and the rest of the ggml family, built with `GGML_RPC`, hand
their layers to a ggml RPC server that lighter runs in-process on the Mac's
Metal backend with ggml's own kernels. Same model, same M1: 1658 tokens/s of
prompt processing and 81 of generation in a container, against 1028 and 45
over Vulkan and 1949 and 110 native. The Vulkan device is the general one;
for ggml this is the fast one, and the gap to native is the round trip per
token.

Linux PyTorch has no Vulkan backend, so `lighter.sh/gpu` gives it nothing;
that is what `lighter.sh/mps` is for.

## Also

- `lighter config --gpu`, `--ane`, `--mps`, `--metal` (`on`/`off`), `--torch-python`.
- Four gates: m9 (vulkaninfo through the CDI device), m10 (an ONNX model
  through the plugin provider, checked against the CPU), m11 (a container's
  PyTorch training a model on `mps`), m12 (llama-bench in a container over
  RPC to Metal, against native).
- The guest kernel gains DRM for virtio-gpu (every SoC display driver pinned
  off): 3 ms of boot, 2 MiB of Image. Linux remains **6.18.52**; the data
  epoch remains **1**.
- The release build links virglrenderer, MoltenVK, ONNX Runtime and ggml
  statically: `make gpu`, `make ane` and `make metal` (`host/gpu/build.sh`,
  `host/ane/build.sh`, `host/metal/build.sh`) build them into `host/out`
  first; without them the VMM builds with the devices stubbed and says so.

## Release artifacts

(recorded at packaging)
