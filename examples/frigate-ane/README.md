# Frigate on the Mac's Neural Engine

Frigate's object detector, running on the Mac's Neural Engine through
`lighter.sh/ane`. The image is Frigate's own with one detector plugin added
(`lighter_ane.py`; everything else is Frigate's own ONNX detector). It runs the
model through lighter's library as an ONNX Runtime custom op, so Frigate's
ONNX Runtime stays as it ships (1.18 in Frigate 0.18); lighter 0.9.2 or newer.

```bash
docker build -t frigate-ane .
docker run -d --name frigate --restart unless-stopped \
  --device lighter.sh/ane=all --shm-size 256m \
  -v ./config:/config -v ./media:/media/frigate -v ./models:/models \
  -p 8971:8971 frigate-ane
```

`config.yml` names the detector (`type: lighter_ane`) and a model; any ONNX
model Frigate's ONNX detector takes works. The one measured is YOLO11n at
320 px, exported with `yolo export model=yolo11n.pt format=onnx imgsz=320
opset=17 simplify=True` and placed in `models/`.

On an M1 with a 768×432 clip at 5 fps, Frigate's own detector statistics:

| detector | inference | detector process CPU |
|---|---:|---:|
| `lighter_ane` (Neural Engine) | 7.6–7.9 ms | 0.8% |
| `onnx` on the container's CPU | 15.1 ms | 36% |

Outputs are within fp16 of the CPU provider (the Neural Engine's precision):
scores within 0.002, the same top candidates, boxes within a pixel.
