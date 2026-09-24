# lighter 0.9.2

The Neural Engine works with the ONNX Runtime an image already ships, back to 1.16; the media engine decodes for more than three times as many cameras, and a crash in the guest's video driver is fixed; and Dockerfiles that fetch a git repository build.

## The Neural Engine from ONNX Runtime 1.16

0.9.1 brought the Neural Engine to any ONNX Runtime from 1.23, the first release that can load a plugin execution provider. Many images ship older ones: Frigate 0.18 has 1.18 and Frigate's development branch 1.22, and upgrading a runtime every other part of an image depends on is not a small ask. The same library now also loads as an ONNX Runtime custom operator, which runtimes have loaded since long before 1.16: `lighter_ane_wrap` turns a model into one node, `lighter.ane:Model`, carrying the original, and `register_custom_ops_library` registers the op that runs the whole model on the Neural Engine. The two routes reach the same host service and cost the same: across Frigate's models on an M1 the custom op on ONNX Runtime 1.22 is within 0.3 ms of the plugin provider on 1.23. `docs/gpu.md` shows both.

The library asks for ONNX Runtime 1.16's C API on this route. Its first version crashed on 1.22, because the run code it shares with the plugin provider called two functions that arrived in 1.23; a runtime hands back a table only as long as its own. A test now reads the source of everything the custom-op route can reach and holds each call inside 1.16's table. A model whose weights are kept in files beside it (ONNX external data) is refused on this route with a clear error, since the host cannot see those files; the plugin provider takes such models.

Gate `m10` runs a model on the custom op under ONNX Runtime 1.16, 1.22 and the newest release, and on the plugin provider under 1.23 and the newest, each checked against the container's CPU.

## Frigate's models

`benchmarks/ane-models.sh` exports each model type Frigate's detector supports with Frigate's own documented recipe, runs it through Frigate's own pipeline (input transform, detector, post-processing) on both routes, and checks every confident detection against the container's CPU: same class, boxes overlapping by 90%, scores within 0.05. On an M1:

| Model | Container CPU | lighter | Where the host ran it |
|---|---:|---:|---|
| YOLOv9-t, 320 | 12.4 ms | 3.3 ms | Neural Engine |
| YOLOv9-s, 640 | 110.2 ms | 13.0 ms | Neural Engine |
| YOLO11n, 320 | 10.9 ms | 4.3 ms | Neural Engine |
| YOLOX-tiny, 416 | 31.3 ms | 12.1 ms | Neural Engine |
| YOLO-NAS-S, 320 | 32.0 ms | 6.6 ms | Neural Engine |
| RF-DETR Nano, 320 | 82.3 ms | 38.3 ms | GPU |
| D-FINE-S, 640 | 119.9 ms | 120.4 ms | the host's CPU: CoreML refuses it |

Milliseconds a detection including Frigate's own pre- and post-processing, the custom op on ONNX Runtime 1.22, every confident detection matched to the CPU's. D-FINE runs correctly and no faster. DEIMv2 is missing because Frigate's documented export of it runs out of memory at 16 GB. Frigate built from its development branch ran each model end to end on a replayed camera clip, keeping up at 5 fps with nothing skipped, and Frigate 0.18's own image ran the example unchanged on its ONNX Runtime 1.18. The full record is `benchmarks/results/ane-models-m1.txt`.

`examples/frigate-ane` no longer upgrades Frigate's ONNX Runtime: the image is Frigate's own with one detector plugin added.

## A detector warmed up on a blank frame reaches the Neural Engine

The host places a model at its first run by timing each unit (Neural Engine, GPU, CPU) on that run's inputs and dropping any that fail. A CoreML partition can fail on one input and run the next: YOLO-NAS carries its own NMS, and on a frame with nothing in it the NMS leaves an empty tensor its CoreML partition will not take. Frigate warms every detector up on a frame of zeros, so YOLO-NAS lost both CoreML candidates there and ran on the host's CPU for good, at a third of the speed; and a model that had reached the Neural Engine failed on any later frame with nothing in it. The CPU candidate now stays loaded when another unit wins and answers any run the winner fails, and a candidate that failed on the first run's input races again on the next runs'. The placement line in the machine log now says why a candidate failed, and a model with only one candidate CoreML will take gets a line too.

## Many cameras on the media engine

A camera sweat test (Frigate replaying a clip as N cameras, each decoded on the media engine and detected on the Neural Engine with YOLO11n at 5 fps, `benchmarks/ane-models/frigate-sweat.sh`) found two faults in the video decoder, both fixed.

Every bitstream buffer the decoder handed out was 4 MiB whatever the application asked for, and each is mapped through a fixed window the decoder has in the guest's address space. ffmpeg takes sixteen of them a stream, so about thirteen cameras filled the window and the next one decoded nothing. The decoder now gives a buffer the size the application asks for (ffmpeg asks 344 KiB for a 896x512 stream), up to the same 4 MiB. On an M5, Frigate went from failing at 16 cameras to running 48, every camera at 5 fps with nothing skipped.

Past that, the guest's video driver could crash. The decoder's events and its replies to commands reach the guest by different queues, and an event that the guest has no buffer for yet waits while replies do not; so a "frame done" for a buffer could arrive after the application had stopped the queue and emptied it. The driver took that buffer back, then took it again when it was next filled, and the next dequeue crashed the guest kernel holding the device, after which every video decode on the machine hung until lighter restarted. Guest patch 0042 ignores a frame done for a buffer the guest no longer has queued. Gate `m13` now runs sixteen clients stopping and restarting decode under load.

Where the camera count stops now, on the same test: an M5 runs 40 cameras on one Frigate detector, whose own process tops out near 290 detections a second, and 48 on two; an M1 runs 20 on one and 32 on two, where the Neural Engine service on the host reaches a full core.

## Also

- Dockerfiles that `ADD` a git repository, and builds whose context is a git URL, failed with `exec: "git": executable file not found`: BuildKit fetches git sources itself, inside the guest, and the guest had no git. It has now. Frigate's documented YOLOv9 export is one such Dockerfile. Gate `m3-docker` builds one.
- Linux remains **6.18.52**, with guest patches 0036 to 0042; the data epoch remains **1**.
