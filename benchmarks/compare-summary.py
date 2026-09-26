#!/usr/bin/env python3
"""The summary compare.sh prints: medians per stage as markdown tables, what
each guest measured itself as, repetitions that disagreed, and whether the
Mac's builds match the image's, from the CSVs and .tree files in one record.

    benchmarks/compare-summary.py <record dir> <machine>
"""
import csv
import re
import statistics
import sys
from pathlib import Path

ORDER = ["native", "lighter", "orbstack", "docker-desktop", "colima", "podman", "apple-container"]
STAGES = {"": "share", "-guest": "guest", "-media": "media"}


def tree(path):
    """A .tree's facts: `key=value` per field, except the tool versions, whose
    values have spaces and take the rest of their line."""
    facts = {}
    for line in path.read_text().splitlines() if path.exists() else []:
        if line.startswith(("native.", "image.")):
            key, _, value = line.partition("=")
            facts[key] = value
            continue
        for field in line.split(" "):
            key, _, value = field.partition("=")
            if value:
                facts[key] = value
    return facts


def build_id(tool, version):
    """What makes two builds the same: llama.cpp's commit, zstd's release."""
    pattern = r"commit ([0-9a-f]{7})" if tool == "llama" else r"v([0-9.]+)"
    found = re.search(pattern, version or "")
    return found.group(1) if found else None


def main(root, machine):
    root = Path(root)
    runs = {}
    for csv_path in sorted(root.glob(f"{machine}-*.csv")):
        name = csv_path.stem[len(machine) + 1:]
        for suffix, stage in sorted(STAGES.items(), key=lambda s: -len(s[0])):
            if suffix and name.endswith(suffix):
                target = name[: -len(suffix)]
                break
        else:
            target, stage = name, "share"
        if target not in ORDER:
            continue
        rows = list(csv.DictReader(csv_path.open()))
        if not rows or "case" not in rows[0]:
            continue
        runs[(target, stage)] = (rows, tree(csv_path.with_suffix(".tree")))

    for stage in ("share", "guest", "media"):
        targets = [t for t in ORDER if (t, stage) in runs]
        if not targets:
            continue
        cases = []
        for t in targets:
            for row in runs[(t, stage)][0]:
                if row["case"] not in cases:
                    cases.append(row["case"])
        print(f"\n**{stage}** (median of each case, as recorded)\n")
        print("| | " + " | ".join(targets) + " |")
        print("|---|" + "---|" * len(targets))
        for case in cases:
            cells = []
            for t in targets:
                values = [float(r["ms"]) for r in runs[(t, stage)][0] if r["case"] == case and re.fullmatch(r"[0-9.]+", r["ms"])]
                failed = any(r["case"] == case and not re.fullmatch(r"[0-9.]+", r["ms"]) for r in runs[(t, stage)][0])
                cells.append("failed" if failed or not values else f"{statistics.median(values):g}")
            print(f"| {case} | " + " | ".join(cells) + " |")

    print("\n**Guests, as measured from inside them**")
    for (t, stage), (_, facts) in sorted(runs.items()):
        if "guest.cpus" in facts:
            print(f"- {t} ({stage}): {facts['guest.cpus']} CPUs, {facts['guest.memory_mib']} MiB")

    spreads = [(t, s, k[len("spread."):], v) for (t, s), (_, f) in sorted(runs.items()) for k, v in f.items() if k.startswith("spread.")]
    print("\n**Repetitions that disagreed by more than 20%**" + ("" if spreads else ": none"))
    for t, s, case, v in spreads:
        print(f"- {t} {s} {case}: {v}")

    native = next((f for (t, _), (_, f) in runs.items() if t == "native" and "native.llama" in f), None)
    image = next((f for (t, _), (_, f) in runs.items() if t != "native" and "image.llama" in f), None)
    if native and image:
        print("\n**The Mac's builds against the image's**")
        for tool in ("llama", "zstd"):
            mac, img = native.get("native." + tool), image.get("image." + tool)
            same = build_id(tool, mac) is not None and build_id(tool, mac) == build_id(tool, img)
            print(f"- {tool}: {'same' if same else 'DIFFERENT'} (Mac: {mac}; image: {img})")

    for extra in ("transcode-quality", "llm-gpu"):
        path = root / f"{machine}-{extra}.csv"
        if path.exists():
            print(f"\n**{extra}** ({path.name})\n")
            print(path.read_text().rstrip())


if __name__ == "__main__":
    main(*sys.argv[1:3])
