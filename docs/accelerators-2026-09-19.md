# lighter 0.7.0: the GPU, the Neural Engine and PyTorch in containers

Status: built. Approved 2026-09-19, spikes passed the same day, all three devices working end to end on the M1 by the evening (gates m9, m10, m11), and a fourth added the same night on the numbers: `lighter.sh/metal`, ggml's RPC server on the Metal backend, because the Vulkan device's generation speed is a MoltenVK ceiling (gate m12). 0.7.0 is the release (`release-notes-0.7.0.md`, `gpu.md`; the latter also records why the fourth device's generation gap was thread scheduling on the Mac and what holds it closed). This note stays as the record of the decisions and the spikes. Three tracks, one release, one qualification. Experiments land as rows in `worklog.md` as they happen.

## What ships

- **Vulkan in containers** (`docker run --device lighter.sh/gpu`): a virtio-gpu device speaking Venus, rendered on the host by virglrenderer over MoltenVK, so llama.cpp, whisper.cpp, ncnn and ONNX Runtime's WebGPU provider reach the M-series GPU from a stock Linux image. The libkrun design, transplanted.
- **The Neural Engine in containers** (`--device lighter.sh/ane`): an ONNX Runtime plugin execution provider in the guest that ships a graph and its tensors over vsock to a host runner, which is ONNX Runtime's own CoreML provider with compute units set to all. Vision and small-model inference at a few watts.
- **PyTorch on the Mac GPU in containers** (`--device lighter.sh/mps`): a guest extension that occupies PyTorch's MPS dispatch key on Linux with one boxed fallback forwarding every operator to the user's own PyTorch on the host, running on MPS. `model.to("mps")` works unchanged. Inference and training, eager and compiled.

Not in scope, stated in the release notes: PyTorch through Vulkan (no such backend exists), a Metal wire protocol (nothing on Linux would consume it), and any bundled PyTorch or CoreML runtime on the host.

## Decisions recorded

1. In-process, one binary. virglrenderer, MoltenVK and `rutabaga_gfx` are statically linked into `lighter`. No helper process; gvproxy was removed for that reason and the release manifest authenticates a fixed file list.
2. DRM goes into the one kernel. `CONFIG_DRM` and `CONFIG_DRM_VIRTIO_GPU` on, added to the build's assertion list. Boot cost is measured and recorded before it is accepted; there is no second kernel.
3. GPU on by default, lazily. The Metal device and renderer exist from the first guest context to the last, so an idle machine pays nothing and the published idle footprint stands.
4. Containers bring their own Mesa. The Venus ICD is `mesa-vulkan-virtio` on Alpine and inside `mesa-vulkan-drivers` on Debian, Ubuntu and Fedora. lighter injects no library across the musl/glibc line; the doctor check proves the path with a throwaway container.
5. The Neural Engine interface is an ONNX Runtime plugin EP (ABI since 1.23), built as one `no_std` shared object on raw syscalls so it loads in any container. Delivered by CDI with a unix socket the guest agent bridges to vsock.
6. Host PyTorch is the user's own, found in an interpreter on the Mac; lighter ships the guest wheel and a host Python module, the way Rosetta is Apple's binary and lighter only arranges it. Guest wheel and host torch must match versions; `lighter doctor` is strict.
7. The guest torch device is named `mps`, by occupying the real MPS key rather than renaming the spare `PrivateUse1` key, which cannot take a reserved name. Fallback if PyTorch refuses: `PrivateUse1` as `lighter` plus a shim resolving `mps` to it.
8. Device names: `lighter.sh/gpu`, `lighter.sh/ane`, `lighter.sh/mps`. Docker's CDI support (on by default since 28.3) is the delivery mechanism for all three; specs are written by the guest's init.
9. All three tracks ship in 0.7.0. There is no slip rule.

## Groundwork the VMM lacks

Track A needs three generic additions before the device: virtio shared-memory region registers in the MMIO transport (today they answer "none"), a reserved GPU aperture in `GuestLayout`, and a late-mapping API that puts foreign host memory (Metal buffers) into guest physical space through the existing `hv_vm_map` wrapper. The virtio-mem hotplug range is the nearest precedent for the third. Tracks B and C need one new vsock port each beside the existing seven, framed like the DNS and UDP streams, a `--bridge` mode in the guest agent, and CDI specs from init.

## Order of work

1. Spikes (below). Track A is go or no-go on the first two.
2. Track A groundwork, then the device, then guest plumbing (kernel, `/dev/dri`, CDI, config, doctor), then gate m9.
3. Track B in parallel with A's device work: protocol, host runner, the EP library, agent bridge, CDI, the Frigate detector as the worked example, gate m10.
4. Track C once B's transport is stable: the guest extension, the host module, version lockstep in doctor, gate m11.
5. Docs: this note kept current, `docs/gpu.md` as the standing document, README rows, release notes. Then the release ritual as `releasing.md` states it: gates on the exact head, a working day on the daily driver, both Macs qualified, `dev` to `main`.

## Gates and numbers

- **m9, Vulkan:** `vulkaninfo` in a container reports "Virtio-GPU Venus (Apple M…)"; llama.cpp's Vulkan backend runs a small model in a container. Recorded against the same model on native Metal on the host and on CPU in the container.
- **m10, Neural Engine:** a YOLO nano runs in a container on the ANE through the EP; latency per inference against the CPU EP in the same container.
- **m11, PyTorch:** three workloads in a container against native MPS: a ResNet-50 fine-tune, a small transformer training loop, a Hugging Face inference pipeline. Coverage is measured by these, never by ATen's operator list.
- README gains "GPU in containers" and "PyTorch on the Mac GPU in containers" rows: lighter yes, OrbStack no, Docker Desktop no, Colima no.
- Every existing gate and record still passes; idle memory and boot time are checked explicitly, since both features touch them.

## Spikes, in order

1. Build virglrenderer with Venus and MoltenVK statically on macOS and compile `rutabaga_gfx` against them. On the M1.
2. Measure the boot cost of DRM plus virtio-gpu in the guest kernel. Kernel built here, measured on the M1.
3. Prove an out-of-tree extension can register kernels, hooks and a device guard for the MPS key on a Linux PyTorch build. On this box.
4. Prove ONNX Runtime loads a `no_std` plugin EP and routes a whole graph through it. On this box.
5. Prove ONNX Runtime's CoreML provider places a YOLO nano on the Neural Engine on the M1, and measure per-inference latency.

## Where the work runs

macOS spikes, VMM code and every gate run on the M1 over Tailscale, with the daily driver on the M5 left alone. Guest kernel and rootfs builds, the EP library, the PyTorch extension and their tests run on this box, where 64 cores and the NVMe stripe make the kernel build cheap; the clone lives on the boot disk, everything under `/mnt/nvme` is ephemeral, and `dev` is pushed often.

## Risks named now

- MoltenVK's Vulkan coverage against what Venus requires; libkrun needed Mesa fixes in 2024 that should be upstream by now. Spike 1 answers it.
- Guest memory accounting: blob resources are host memory in guest physical space, outside the balloon, virtio-mem and the purgable pages. `lighter status` must report them and the reclaim paths must ignore them.
- x86-64 images under Rosetta with the Venus ICD are untested and are not a release claim.
- Track C's eager path is one round trip per operator; acceptable on lighter's vsock, competitive only under `torch.compile`. The numbers from m11 decide what the release notes promise.
- Aliasing and in-place operators are where a remote tensor backend goes wrong; PyTorch's OpenReg reference is the pattern to follow.

## Spike results (2026-09-19)

1. **Venus on macOS with Command Line Tools alone.** Upstream virglrenderer (`32dac0c`) builds Venus-only and static on the M1 with `-Dvrend=false -Dvenus=true -Drender-server-mode=thread -Drender-server-worker=thread`, no Homebrew and no Xcode: meson and ninja from pip, MoltenVK from the Khronos release tarball. Two things to know: the venus-protocol subproject must be reachable as `venus-protocol/vulkan_metal.h` (a symlink until upstream settles the path), and `VIRGL_RENDERER_RENDER_SERVER` must be in the init flags or the capset stays empty. A Rust binary linking `libvirglrenderer.a`, `libvirgl.a` and `libmesa.a` plus Metal and Foundation initialises, fills the Venus capset (160 bytes) and creates a Venus context, so the renderer runs in-process as a thread. `spikes/accelerators-2026-09-19/venus-link`.
2. **DRM in the guest kernel costs 3 ms.** 6.18.52 with `DRM`, `DRM_VIRTIO_GPU` and every SoC display driver pinned off (the defconfig turns on seventy of them, and Qualcomm's needs python3 just to build): the Image grows 2 MiB and `Run /init` moves from 88.6 ms to 91.6 ms, median of seven boots each on the M1. Accepted; the option list is in `guest/kernel/lighter.config`.
3. **PyTorch's MPS key can be occupied on Linux.** An out-of-tree extension registers an allocator, a device guard, the `MPSHooks` class under the name PyTorch looks up (so `torch.backends.mps.is_available()` and `torch.accelerator.current_accelerator()` answer truthfully), factory and copy kernels, and one boxed fallback for everything else; `copy_` must be registered on the key because `copy_impl`'s dispatch stub is compiled out on Linux. Forward, backward and an Adam loop converge on `torch.device("mps")` against CPU PyTorch 2.14. `spikes/accelerators-2026-09-19/torch-mps`.
4. **ONNX Runtime loads a plugin EP written in Rust.** It advertises an NPU, claims every node without a subgraph as one fused unit, re-serialises the fused graph to ONNX bytes through the public graph API (no serialisation call exists, so the plugin carries a 200-line protobuf writer), and forwards runs over a unix socket. Outputs match the CPU provider bit for bit; 0.42 ms a run for a small CNN including the socket. `spikes/accelerators-2026-09-19/ort-ep`.
5. **The Neural Engine takes ResNet-50 at 1.76 ms.** ONNX Runtime's CoreML provider on the M1 with `ModelFormat=NeuralNetwork`: CPU 30.0 ms, CoreML on CPU 16.4, on GPU 7.9, on the Neural Engine 1.76. The `MLProgram` format never reaches the ANE for this model (E5RT rejects it as unbounded whatever the declared shapes), so the host runner uses NeuralNetwork first. Batch dimensions must be fixed for the ANE; the host binds them from the first run's shapes. `spikes/accelerators-2026-09-19/ane.py`.

Two design facts the spikes settled: blob mappings on Apple silicon are 16 KiB pages, so the guest kernel's host-visible allocator is patched to 16 KiB alignment (lighter owns the kernel; stock Mesa then works unchanged) and the host rounds sizes the same way; and the ANE device sends the model at the first run rather than at compile, because that is when the shapes are known.
