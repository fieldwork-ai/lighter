# Immich on the Mac's Neural Engine

Immich's machine learning (smart search's CLIP, face detection and recognition) running on the Mac's Neural Engine through `lighter.sh/ane`. Immich's own image, unchanged: `sitecustomize.py`, mounted into the container, hands Immich's ONNX Runtime sessions lighter's provider ahead of the CPU. Needs lighter 0.9.2 or newer, and Immich's ML image with ONNX Runtime 1.23 or newer (every release since it moved there).

In Immich's `docker-compose.yml`, the `immich-machine-learning` service gains three lines:

```yaml
  immich-machine-learning:
    image: ghcr.io/immich-app/immich-machine-learning:${IMMICH_VERSION:-release}
    devices:
      - lighter.sh/ane=all
    environment:
      - PYTHONPATH=/lighter
    volumes:
      - model-cache:/cache
      - ./immich-ane:/lighter:ro     # this directory
```

On an M1, Immich's `/predict` on a photo with six faces, after warm-up, median of twenty requests:

| | CPU (as Immich ships) | `lighter.sh/ane` |
|---|---:|---:|
| CLIP image embedding (ViT-B-32) | 66 ms | 26 ms |
| face detection and recognition (buffalo_l) | 680 ms | 42 ms |

lighter times every model on the Neural Engine, the GPU and the CPU at its first run and keeps the fastest; both went to the Neural Engine (9.5 and 17.5 ms against 17.4 and 44.9 on the GPU and 93 and 337 on the Mac's CPU, the line in `~/.lighter/machine.log`). The results agree with the CPU's to the Neural Engine's precision: CLIP embeddings to about six decimal places, the same face boxes within a pixel. The first request for each model compiles it for CoreML, some seconds, and after that it is cached.

Immich's own log still says `CPUExecutionProvider`: the provider is added under it. Without the device, or on an older ONNX Runtime, the shim does nothing and Immich runs as it ships.
