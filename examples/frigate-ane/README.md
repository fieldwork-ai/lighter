# Frigate on the Mac's Neural Engine

Frigate's object detector, running on the Mac's Neural Engine through
`lighter.sh/ane`. Frigate 0.18 ships ONNX Runtime 1.18, which predates the
plugin provider API, so the image is derived: the runtime upgraded and one
detector plugin added (`lighter_ane.py`, forty lines; everything else is
Frigate's own ONNX detector).

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
