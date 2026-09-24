# Judges one frigate-e2e.sh run from Frigate's stats, its hardware probe and
# the line its log wrote when the model loaded.
import json
import sys

stats, probe, loaded = json.loads(sys.argv[1]), sys.argv[2], sys.argv[3]
camera = stats["cameras"]["clip"]
detectors = list(stats["detectors"].values())
inference = detectors[0]["inference_speed"] if detectors else 0
ok = (
    camera["camera_fps"] >= 4.5
    and camera["skipped_fps"] == 0
    and inference > 0
    and "Neural Engine" in loaded
    and ("lighter_ane" in probe or "Apple Neural Engine" in probe)
)
print(
    ("ok  " if ok else "FAIL")
    + f" camera {camera['camera_fps']} fps, detect {camera['detection_fps']} fps,"
    + f" skipped {camera['skipped_fps']}, inference {inference} ms"
)
