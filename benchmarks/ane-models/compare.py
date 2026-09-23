# Compares the detections matrix.py recorded on the CPU and on the Neural
# Engine, after Frigate's post-processing. A confident detection (score >=
# CONFIDENT) on either side must be matched on the other: same class, IoU >=
# MIN_IOU, score within MAX_SCORE_DIFF. Frigate reports at 0.4 and up, so a
# detection near that line may appear on one side only; confident ones may not.
#
#   python3 compare.py out.json [...]      prints one line per file, exits 1 on any mismatch
import json
import sys

CONFIDENT = 0.5
MIN_IOU = 0.9
MAX_SCORE_DIFF = 0.05


def iou(a, b):
    # Frigate's rows are [class, score, y_min, x_min, y_max, x_max].
    y0, x0, y1, x1 = max(a[2], b[2]), max(a[3], b[3]), min(a[4], b[4]), min(a[5], b[5])
    inter = max(0.0, y1 - y0) * max(0.0, x1 - x0)
    area = lambda r: max(0.0, r[4] - r[2]) * max(0.0, r[5] - r[3])
    union = area(a) + area(b) - inter
    return inter / union if union > 0 else 0.0


def unmatched(these, others):
    """Confident detections in `these` with no counterpart in `others`."""
    bad = []
    for d in these:
        if d[1] < CONFIDENT:
            continue
        best = max(
            (iou(d, o) for o in others if int(o[0]) == int(d[0]) and abs(o[1] - d[1]) <= MAX_SCORE_DIFF),
            default=0.0,
        )
        if best < MIN_IOU:
            bad.append(d)
    return bad


failed = False
for path in sys.argv[1:]:
    r = json.load(open(path))
    confident = mismatched = 0
    for image, cpu in r["cpu"].items():
        cpu = [d for d in cpu if d[1] > 0]
        ane = [d for d in r["ane"][image] if d[1] > 0]
        confident += sum(1 for d in cpu if d[1] >= CONFIDENT)
        bad = unmatched(cpu, ane) + unmatched(ane, cpu)
        mismatched += len(bad)
        for d in bad:
            print(f"  {r['model']} {r['route']} {image}: unmatched {['%.3f' % v for v in d]}")
    ok = mismatched == 0 and confident > 0
    failed |= not ok
    print(
        f"{'ok  ' if ok else 'FAIL'} {r['model']:<26} {r['route']:<6} ORT {r['ort']:<7} "
        f"cpu {r['cpu_ms']:6.2f} ms  ane {r['ane_ms']:6.2f} ms  load {r['load_ms']:6.0f} ms  "
        f"{confident} confident detections, {mismatched} unmatched"
    )
sys.exit(1 if failed else 0)
