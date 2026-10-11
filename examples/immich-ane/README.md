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

Immich 3 prepares each model once, saving a copy optimized for the CPU, and runs the copy. A session that saves its graph cannot hold another provider's compiled nodes, so the shim leaves that one alone and gives lighter's provider to the session that runs the copy. An older copy of this file makes Immich 3 fail to load every model (`Unable to serialize model as it contains compiled nodes` in its log); take the current one. On an M5 Ultra, Immich 3's `/predict` with CLIP and face detection took 26 ms against 146 ms as Immich ships, median of twenty.

## Checking it works

- `~/.lighter/machine.log` has a `neural engine model placed` line for each model the container loaded, naming where it runs (`chosen=`) and what each candidate took. The fastest can be the GPU or the Mac's CPU rather than the Neural Engine; any of them is the Mac, not the container.
- A monitor that shows only the GPU shows nothing for a model on the Neural Engine. In Activity Monitor, the work is lighter's.
- `LIGHTER_ANE` is set in the container's own processes, not in `docker exec`'s: `docker exec immich_machine_learning sh -c 'tr "\0" "\n" < /proc/1/environ | grep LIGHTER'`.
- Thumbnails, video transcoding and metadata run in `immich-server`, on the container's CPU, whatever happens to machine learning.
