#!/usr/bin/env python3
"""Validate one completed release suite and retain every observation, without a VM."""

import argparse
from collections import defaultdict
import csv
import gzip
import hashlib
import json
import math
from pathlib import Path
import statistics


def unit(case):
    if case.startswith("memory-"):
        return "MiB"
    if case == "power-cpu-ms-per-s":
        return "CPU ms/s"
    if case == "power-energy-x10":
        return "energy x0.1"
    if case.startswith("power-"):
        return "wakeups/s"
    if case == "net-connect-rate":
        return "connections/s"
    if case.startswith("net-http") or case == "net-dns":
        return "us"
    return "Mbit/s" if case.startswith("net-") else "ms"


def summarize(root):
    assert (root / "exit-code").read_text().strip() == "0", "incomplete suite"
    env = json.loads((root / "environment.json").read_text())
    assert env["primary"] == "one full suite" and env["repetitions"] == 3
    entries = json.loads((root / "records.json").read_text())
    assert len(entries) == 3
    assert {e["label"].rsplit("-", 1)[-1] for e in entries} == {
        "share", "guest", "amd64"
    }
    summary = {
        "source": env["source"],
        "runtime_source": env["runtime_source"],
        "profile": env["profile"],
        "note": "One suite, three observations per timed case; sample CV is within "
        "this suite, not an established run-to-run variance bound. Memory and "
        "power are single observation windows.",
        "stages": {},
    }
    for entry in entries:
        label = entry["label"]
        stage = label.rsplit("-", 1)[-1]
        path = root / (label + ".csv")
        assert entry["runtime_source"] == env["runtime_source"]
        assert hashlib.sha256(path.read_bytes()).hexdigest() == entry["sha256"]
        guard_path = root / (label + "-guard.jsonl")
        if guard_path.exists():
            guard_text = guard_path.read_text()
        else:
            with gzip.open(guard_path.with_suffix(".jsonl.gz"), "rt") as handle:
                guard_text = handle.read()
        guard = [json.loads(line) for line in guard_text.splitlines()]
        assert guard[-1] == dict(result="complete", error=None, exit_code=0)
        assert not any(row.get("unexpected") for row in guard)
        quiet = [row for row in guard if row.get("phase") == "quiet"]
        assert len(quiet) >= 6
        assert all(row["machine_cpu_percent"] <= 5 for row in quiet[-6:])
        assert quiet[-1]["time"] - quiet[-6]["time"] >= 49

        values, reps = defaultdict(list), defaultdict(list)
        with path.open() as handle:
            for row in csv.DictReader(handle):
                value = float(row["ms"])
                assert math.isfinite(value) and value >= 0
                values[row["case"]].append(value)
                reps[row["case"]].append(int(row["rep"]))
        expected = set()
        for case in entry["cases"].split():
            expected.update({
                "memory": ["memory-peak", "memory-after-15s", "memory-after-60s"],
                "power-idle": ["power-cpu-ms-per-s", "power-wakeups-per-s"],
                "boot": ["boot-docker", "boot-first-container", "memory-idle"],
            }.get(case, [case]))
        extras = {"net-http-p99", "power-pkg-idle-wakeups-per-s", "power-energy-x10"}
        assert expected <= set(values) <= expected | extras
        summary["stages"][stage] = {}
        for case, observations in values.items():
            count = 1 if case.startswith(("memory-", "power-")) else 3
            assert len(observations) == count
            assert sorted(reps[case]) == list(range(1, count + 1))
            mean = statistics.mean(observations)
            cv = 100 * statistics.stdev(observations) / mean if count > 1 and mean else None
            summary["stages"][stage][case] = {
                "unit": unit(case),
                "observations": observations,
                "median": statistics.median(observations),
                "sample_cv_percent": cv,
            }
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("records", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    summary = summarize(args.records)
    args.out.write_text(json.dumps(summary, indent=2) + "\n")
    print("Validated one suite:", ", ".join(summary["stages"]))


if __name__ == "__main__":
    main()
