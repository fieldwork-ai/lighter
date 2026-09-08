#!/usr/bin/env python3
"""Regenerate 0.5.1 tables from archived measurements; never start a VM."""

import argparse
import csv
from datetime import datetime
import gzip
import hashlib
import json
import math
from pathlib import Path
import statistics as stats


def read_json(path):
    return json.loads(path.read_text())


def observations(path):
    with gzip.open(path, "rt") as source:
        return [json.loads(line) for line in source]


def csv_cases(path):
    cases = {}
    with path.open() as source:
        for row in csv.DictReader(source):
            value = float(row["ms"])
            assert math.isfinite(value) and value >= 0, path
            cases.setdefault(row["case"], []).append(value)
    for case, values in cases.items():
        expected = 1 if case.startswith(("memory-", "power-")) else 3
        assert len(values) == expected, (path, case, len(values))
    return cases


def cv(values):
    mean = stats.mean(values)
    return 100 * stats.stdev(values) / mean if len(values) > 1 and mean else None


def unit(case):
    if case.startswith("memory-"):
        return "MiB"
    if case == "power-cpu-ms-per-s":
        return "CPU ms/s"
    if case.startswith("power-"):
        return "wakeups/s"
    if case == "net-connect-rate":
        return "connections/s"
    if case.startswith("net-http") or case == "net-dns":
        return "µs"
    if case.startswith("net-"):
        return "Mbit/s"
    return "ms"


def signed_boot(base, saved):
    assert (base / "exit-code").read_text().strip() == "0"
    arms, profiles, images = [], [], []
    for index, version in enumerate(["050", "051", "051", "050"], 1):
        directory = base / f"{index}-{version}"
        assert (directory / "exit-code").read_text().strip() == "0"
        env = read_json(directory / "environment.json")
        assert env["mode"] == "default" and env["saved_container"] == saved
        assert not env["timing"]
        profiles.append([env[k] for k in ["cpus", "memory_mib", "disk_gib", "platform"]])
        images.append(read_json(directory / "4096-image.json")["Id"])
        rows = read_json(directory / "results.json")
        assert len(rows) == 5 and {r["rep"] for r in rows} == set(range(1, 6))
        assert {r["memory_mib"] for r in rows} == {4096}
        guard = observations(base / f"{index}-{version}-guard.jsonl.gz")
        assert guard and all(not r.get("unexpected") for r in guard)
        quiet = [r for r in guard if r.get("phase") == "quiet"]
        assert len(quiet) >= 6
        assert all(r["machine_cpu_percent"] <= 5 for r in quiet[-6:])
        metrics = {}
        for metric in ["docker_ms", "first_container_ms"]:
            values = [r[metric] for r in rows]
            assert all(math.isfinite(v) and v > 0 for v in values)
            metrics[metric] = dict(median_ms=stats.median(values), cv_percent=cv(values))
        arms.append(dict(index=index, version="0.5." + version[-1], metrics=metrics))
    assert all(p == profiles[0] for p in profiles) and len(set(images)) == 1
    changes = {}
    for metric in ["docker_ms", "first_container_ms"]:
        values = [r["metrics"][metric]["median_ms"] for r in arms]
        old, new = math.sqrt(values[0] * values[3]), math.sqrt(values[1] * values[2])
        changes[metric] = dict(old_ms=old, new_ms=new, change_percent=100 * (new / old - 1))
    return dict(arms=arms, changes=changes, image_id=images[0], profile=profiles[0])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("records", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = args.records
    if (root / "manifest.json").exists():
        for name, expected in read_json(root / "manifest.json").items():
            assert hashlib.sha256((root / name).read_bytes()).hexdigest() == expected, name
    full = [root / name for name in ["m1-full", "m1-full-run2", "m1-full-run3"]]
    envs = [read_json(p / "environment.json") for p in full]
    assert all((p / "exit-code").read_text().strip() == "0" for p in full)
    assert len({e["runtime_source"] for e in envs}) == 1
    assert all(e["artifacts"] == envs[0]["artifacts"] for e in envs)
    summary = dict(runtime_source=envs[0]["runtime_source"], full={}, storage_abba=[])
    lines = [
        "# 0.5.1 performance measurements", "",
        "Three complete M1 suites use the same frozen runtime, eight vCPUs, 4 GiB guest RAM, a 128 GiB sparse disk and pinned package tools and images. The first valid suite remains the primary record. Each timing is the median of three successful repetitions; memory and idle-power rows are sampling windows. Between-run CV is sample standard deviation divided by the arithmetic mean of these three values.", "",
        "Each stage requires six aggregate CPU observations at most 5%, ten seconds apart, and rejects unexpected VMs throughout. CPU and daemon observations remain available for assessing interference during workloads. No daemon was reset. The ABBA storage follow-up ran between full suites one and two. Warm-up/setup exit statuses were not retained by the inherited protocol; the archived measured-case diagnostics cannot validate those statuses retrospectively.", "",
        "The M5 remained shared. No full M5 suite was run for 0.5.1; the README's complete comparison remains the 0.5.0 M5 record.", "",
        "[Repeatability](REPEATABILITY.md)", "",
    ]
    for stage in ["share", "guest", "amd64"]:
        data = [csv_cases(next(p.glob(f"*1-{stage}.csv"))) for p in full]
        assert all(d.keys() == data[0].keys() for d in data)
        lines += [f"## M1 — {stage}", "", "| Metric | Unit | Primary | Run 2 | Run 3 | Between-run CV |", "|---|---|---:|---:|---:|---:|"]
        summary["full"][stage] = {}
        for case in data[0]:
            values = [stats.median(d[case]) for d in data]
            variation = cv(values)
            summary["full"][stage][case] = dict(medians=values, cv_percent=variation)
            display = f"{variation:.2f}%" if variation is not None else "—"
            lines.append(f"| {case} | {unit(case)} | " + " | ".join(f"{v:g}" for v in values) + f" | {display} |")
        lines.append("")
    abba = root / "m1-abba"
    assert (abba / "exit-code").read_text().strip() == "0"
    data = [csv_cases(next(abba.glob(f"*abba-{i}-*.csv"))) for i in range(1, 5)]
    lines += ["## Matched storage comparison", "", "The first suite appeared slower than the historical 0.5.0 record on several share workloads, prompting this follow-up. Both VMMs were rebuilt with the same compiler, using each version's guest payload. Order is 0.5.0 / 0.5.1 / 0.5.1 / 0.5.0, with three repetitions per arm. Change is the ratio of geometric means of arm medians minus one; positive means longer elapsed time.", "", "| Case | Ordered arm medians (ms) | 0.5.1 time change |", "|---|---|---:|"]
    for case in data[0]:
        values = [stats.median(d[case]) for d in data]
        change = 100 * (math.sqrt(values[1] * values[2] / (values[0] * values[3])) - 1)
        summary["storage_abba"].append(dict(case=case, medians=values, change_percent=change))
        lines.append("| " + case + " | " + ", ".join(f"{v:g}" for v in values) + f" | {change:+.2f}% |")
    lines += ["", "The broad historical install/copy slowdown did not reproduce. The small increases in find and removal remain in the record. Two arms per version cannot establish a precise causal effect, and measured variation is not an automatic threshold for declaring a difference noise.", ""]
    summary["signed_boot"] = {}
    lines += ["## Startup measurement protocols", "", "The full suite starts the checkout CLI with guest/out and a private development app, using Node wall-clock timestamps. The signed comparison starts the shipped Developer ID app and uses Python monotonic timestamps. Their absolute times are separate records; use the signed archive comparison below for the like-for-like release startup claim.", ""]
    for saved in [False, True]:
        name = "saved" if saved else "image-only"
        base = root / "m1-signed-boot"
        if saved:
            base /= "saved"
        result = signed_boot(base, saved)
        summary["signed_boot"][name] = result
        lines += [f"## Signed archive startup — {name}", "", "The published 0.5.0 and final 0.5.1 archives use identical 8-vCPU, 4-GiB, 128-GiB profiles and the same Alpine image. Each arm has one untimed preparation round and five retained cold starts. An outer guard enforces the quiet-host and VM checks; the inner recorder does not itself enforce quietness.", "", "| Arm | Version | Docker median | Within-arm CV | First-container median | Within-arm CV |", "|---|---|---:|---:|---:|---:|"]
        for arm in result["arms"]:
            d, f = [arm["metrics"][k] for k in ["docker_ms", "first_container_ms"]]
            lines.append(f"| {arm['index']} | {arm['version']} | {d['median_ms']:.1f} ms | {d['cv_percent']:.2f}% | {f['median_ms']:.1f} ms | {f['cv_percent']:.2f}% |")
        lines += ["", "Using geometric means of the two arm medians per version:", ""]
        for label, metric in [("Docker readiness", "docker_ms"), ("First-container completion", "first_container_ms")]:
            r = result["changes"][metric]
            lines.append(f"- {label}: {r['old_ms']:.1f} → {r['new_ms']:.1f} ms ({r['change_percent']:+.2f}% elapsed time).")
        lines += ["", "A saved, stopped Alpine container triggers the pre-Docker memory gate in this profile. It isolates that gate's cost; it does not measure restoring an application or kind cluster." if saved else "These image-only starts do not take the saved-container memory gate.", ""]
    daemon = []
    for p in [*full, abba, root / "m1-post-full"]:
        daemon.extend(observations(p / "daemon.jsonl.gz"))
    daemon.sort(key=lambda r: r["utc"])
    footprint = [r["footprint_mib"] for r in daemon if r.get("footprint_mib") is not None]
    post = observations(root / "m1-post-full/daemon.jsonl.gz")
    post_seconds = (datetime.fromisoformat(post[-1]["utc"]) - datetime.fromisoformat(post[0]["utc"])).total_seconds()
    assert post_seconds >= 590, post_seconds
    invalid_cpu = sum(r.get("cpu_percent") is not None and not 0 <= r["cpu_percent"] <= 800 for r in daemon)
    cpu = [r["cpu_percent"] for r in post if r.get("cpu_percent") is not None and 0 <= r["cpu_percent"] <= 800]
    assert cpu and (root / "m1-post-full/exit-code").read_text().strip() == "0"
    summary["daemon"] = dict(samples=len(daemon), pids=sorted({r["pid"] for r in daemon if r.get("pid")}), initial_mib=footprint[0], peak_mib=max(footprint), final_mib=footprint[-1], post_cpu_median=stats.median(cpu), post_cpu_max=max(cpu), post_sample_span_seconds=post_seconds, invalid_cpu_samples=invalid_cpu, collection_errors=sum("error" in r for r in daemon))
    d = summary["daemon"]
    lines += ["## Filesystem-daemon observation", "", f"Across the three full suites, storage ABBA and ten-minute post-suite observation, {d['samples']} observations tracked PID(s) {d['pids']}. Sampled footprint was initially {d['initial_mib']:g} MiB, peaked at {d['peak_mib']:g} MiB and ended at {d['final_mib']:g} MiB. Post-suite CPU had median {d['post_cpu_median']:g}% and maximum {d['post_cpu_max']:g}% of one core. There were {d['collection_errors']} collection errors and {d['invalid_cpu_samples']} invalid CPU observations.", "", "Sampling every five seconds can miss shorter peaks, and memory inherits top's display rounding. The ten-minute window precedes the later signed-archive tests. These observations do not establish a fix for an earlier incident whose trigger was not reproduced.", "", "## Interpretation and selection", "", "Full-suite attempt 1 stopped before measurements because the checkout lacked the published v0.5.0 tag. Attempt 2 is the first valid complete suite and remains primary. No slow measured repetition was discarded. The paired storage follow-up was selected after inspecting the historical difference; both the original observation and follow-up remain available.", "", "Docker probes include command execution and a 50 ms interval after failures. First-container completion includes any remaining memory preparation wait. These results describe the tested host, configuration and workload; they are not universal startup times or variance bounds. Configured guest RAM is distinct from macOS process footprint, which includes host overhead and compressed-memory charges.", ""]
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    (args.output / "RELEASE-0.5.1.md").write_text("\n".join(lines))
    print("Validated and summarized three full suites, storage ABBA, two signed boot profiles and daemon observations")


if __name__ == "__main__":
    main()
