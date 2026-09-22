#!/usr/bin/env python3
"""Validate and summarize the frozen demand-backed 0.5.1 release records."""

import argparse
import csv
from datetime import datetime
import gzip
import hashlib
import json
import math
from pathlib import Path
import statistics as stats


def read(path):
    return json.loads(path.read_text())


def rows(path):
    opener = gzip.open if path.suffix == ".gz" else open
    with opener(path, "rt") as source:
        return [json.loads(line) for line in source]


def recorded(path):
    return path if path.exists() else path.with_suffix(path.suffix + ".gz")


def cv(values):
    return 100 * stats.stdev(values) / stats.mean(values) if stats.mean(values) else 0


def guard(path):
    data = rows(recorded(path))
    assert data[-1] == dict(result="complete", error=None, exit_code=0), path
    assert not any(r.get("unexpected") for r in data), path
    quiet = [r for r in data if r.get("phase") == "quiet"]
    assert len(quiet) >= 6 and all(r["machine_cpu_percent"] <= 5 for r in quiet[-6:]), path


def success(path):
    assert (path / "exit-code").read_text().strip() == "0", path


def cases(path):
    result = {}
    with path.open() as source:
        for row in csv.DictReader(source):
            value = float(row["ms"])
            assert math.isfinite(value) and value >= 0, path
            result.setdefault(row["case"], []).append(value)
    for name, values in result.items():
        assert len(values) == (1 if name.startswith(("memory-", "power-")) else 3), (path, name)
    return result


def unit(name):
    if name.startswith("memory-"):
        return "MiB"
    if name == "power-cpu-ms-per-s":
        return "CPU ms/s"
    if name.startswith("power-"):
        return "wakeups/s"
    if name == "net-connect-rate":
        return "connections/s"
    if name.startswith("net-http") or name == "net-dns":
        return "µs"
    return "Mbit/s" if name.startswith("net-") else "ms"


def signed(base, memory, saved):
    success(base)
    arms, images, profiles, binaries, payloads = [], [], [], {}, {}
    for index, version in enumerate(["050", "051", "051", "050"], 1):
        directory = base / f"{index}-{version}"
        success(directory)
        guard(base / f"{index}-{version}-guard.jsonl")
        env = read(directory / "environment.json")
        assert env["mode"] == "default" and not env["timing"]
        assert env["saved_container"] == saved
        profile = [env[k] for k in ["cpus", "memory_mib", "disk_gib", "platform"]]
        assert profile[:3] == [8, [memory], 128]
        profiles.append(profile)
        images.append(read(directory / f"{memory}-image.json")["Id"])
        binaries.setdefault(version, set()).add(env["cli_sha256"])
        payloads.setdefault(version, set()).add(json.dumps(env["payload"], sort_keys=True))
        data = read(directory / "results.json")
        assert len(data) == 5 and {r["rep"] for r in data} == set(range(1, 6))
        assert {r["memory_mib"] for r in data} == {memory}
        metrics = {}
        for metric in ["docker_ms", "first_container_ms"]:
            values = [r[metric] for r in data]
            assert all(math.isfinite(v) and v > 0 for v in values)
            metrics[metric] = dict(median_ms=stats.median(values), cv_percent=cv(values))
        arms.append(dict(version=version, metrics=metrics))
    assert all(p == profiles[0] for p in profiles) and len(set(images)) == 1
    assert all(len(v) == 1 for v in [*binaries.values(), *payloads.values()])
    changes = {}
    for metric in ["docker_ms", "first_container_ms"]:
        values = [r["metrics"][metric]["median_ms"] for r in arms]
        old, new = math.sqrt(values[0] * values[3]), math.sqrt(values[1] * values[2])
        changes[metric] = dict(old_ms=old, new_ms=new, change_percent=100 * (new / old - 1))
    return dict(arms=arms, changes=changes, image_id=images[0], profile=profiles[0])


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("records", type=Path)
    p.add_argument("--output", type=Path, required=True)
    a = p.parse_args()
    for name, expected in read(a.records / "manifest.json").items():
        assert hashlib.sha256((a.records / name).read_bytes()).hexdigest() == expected, name
    summary = {}
    lines = ["# 0.5.1 demand-backed release measurements", "",
        "Three full suites per host use frozen runtime b303dc2, eight vCPUs, a 128 GiB sparse disk, and pinned tools and images. Guest RAM is 16 GiB on M5 and 4 GiB on M1. The first valid suite is primary by execution order. Each timing is a median of three retained repetitions; memory and idle-power rows are observation windows. CV uses sample standard deviation divided by the arithmetic mean of the three run values.", "",
        "Before each stage, the guard requires six aggregate CPU samples at most 5%, ten seconds apart, and rejects competing VMs throughout. No filesystem daemon is reset. The inherited protocol does not retain untimed setup/warm-up exit statuses; measured-case diagnostics cannot reconstruct them. Results are specific to the host, workload and session, not universal variance bounds.", ""]
    for host, memory in [("m5", 16384), ("m1", 4096)]:
        root = a.records / host
        success(root)
        full = [root / f"full-{i}" for i in [1, 2, 3]]
        envs = [read(f / "environment.json") for f in full]
        assert all(e["runtime_source"] == "b303dc2d87212f52ac3e57afb4e4629e360fd628" for e in envs)
        assert all(e["artifacts"] == envs[0]["artifacts"] and e["profile"] == envs[0]["profile"] for e in envs)
        assert envs[0]["profile"] == dict(BENCH_CPUS="8", BENCH_MEMORY_MIB=str(memory), BENCH_DISK_GIB="128")
        report = dict(full={}, signed={})
        if host == "m5":
            lines += ["An earlier M5 attempt completed its share stage but timed out waiting for a quiet guest-stage preflight. It is retained as incomplete and is not a full-suite input. The three complete suites use the foreground setup established before recording: display/user-active assertions and hidden terminal/editor/Activity Monitor windows. No CPU threshold was relaxed.", ""]
        lines += ["Wallpaper and photo/media analysis processes were temporarily paused, with exact process identities recorded and automatic restoration after qualification. The filesystem daemon was not paused or reset. Guest configuration, runtime artifacts and timing thresholds stayed fixed.", ""]
        for f in full:
            success(f)
        for stage in ["share", "guest", "amd64"]:
            paths = [next(f.glob(f"*-{stage}.csv")) for f in full]
            for f, path in zip(full, paths):
                guard(f / (path.stem + "-guard.jsonl"))
            data = [cases(path) for path in paths]
            assert all(d.keys() == data[0].keys() for d in data)
            lines += [f"## {host.upper()} — {stage}", "", "| Metric | Unit | Primary | Run 2 | Run 3 | Between-run CV |", "|---|---|---:|---:|---:|---:|"]
            report["full"][stage] = {}
            for name in data[0]:
                values = [stats.median(d[name]) for d in data]
                variation = cv(values)
                report["full"][stage][name] = dict(medians=values, cv_percent=variation)
                lines.append(f"| {name} | {unit(name)} | " + " | ".join(f"{v:g}" for v in values) + f" | {variation:.2f}% |")
            lines.append("")
        lines += [f"## {host.upper()} — signed archive startup", "", "The published 0.5.0 and new 0.5.1 archives run in ABBA order with five cold starts per arm and the same Alpine image. Image-only and saved-stopped-container profiles are separate. These Python monotonic measurements use the shipped app; the full suite uses a checkout development app and Node timestamps. Do not pool their absolute times.", ""]
        for saved in [False, True]:
            name = "saved" if saved else "image-only"
            base = root / "signed-boot"
            if saved:
                base /= "saved"
            result = signed(base, memory, saved)
            report["signed"][name] = result
            lines += [f"### {name}", "", "| Arm | Version | Docker ready | Within-arm CV | First container | Within-arm CV |", "|---|---|---:|---:|---:|---:|"]
            for i, arm in enumerate(result["arms"], 1):
                d, f = [arm["metrics"][k] for k in ["docker_ms", "first_container_ms"]]
                lines.append(f"| {i} | 0.5.{arm['version'][-1]} | {d['median_ms']:.1f} ms | {d['cv_percent']:.2f}% | {f['median_ms']:.1f} ms | {f['cv_percent']:.2f}% |")
            lines.append("")
            for label, metric in [("Docker readiness", "docker_ms"), ("First-container completion", "first_container_ms")]:
                r = result["changes"][metric]
                lines.append(f"- {label}: {r['old_ms']:.1f} → {r['new_ms']:.1f} ms ({r['change_percent']:+.2f}% elapsed time), using geometric means of arm medians.")
            lines += ["", "The saved profile exercises the pre-Docker full-online-memory gate. It does not measure restoring a complete application or cluster." if saved else "The image-only profile has no saved containers.", ""]
        post = root / "post-suite"
        success(post)
        data = [r for f in [*full, post] for r in rows(recorded(f / "daemon.jsonl"))]
        data.sort(key=lambda r: r["utc"])
        after = rows(recorded(post / "daemon.jsonl"))
        duration = (datetime.fromisoformat(after[-1]["utc"]) - datetime.fromisoformat(after[0]["utc"])).total_seconds()
        assert duration >= 590
        errors = sum("error" in r for r in data)
        limit = 800 if host == "m1" else 1800
        invalid = sum(r.get("cpu_percent") is not None and not 0 <= r["cpu_percent"] <= limit for r in data)
        footprints = [r["footprint_mib"] for r in data if r.get("footprint_mib") is not None]
        cpu = [r["cpu_percent"] for r in after if r.get("cpu_percent") is not None and 0 <= r["cpu_percent"] <= limit]
        assert cpu and footprints
        daemon = dict(samples=len(data), pids=sorted({r["pid"] for r in data if r.get("pid")}), first_mib=footprints[0], peak_mib=max(footprints), last_mib=footprints[-1], post_cpu_median=stats.median(cpu), post_cpu_max=max(cpu), collection_errors=errors, invalid_cpu_samples=invalid, post_seconds=duration)
        report["daemon"] = daemon
        lines += [f"## {host.upper()} — filesystem daemon", "", f"{daemon['samples']} observations tracked PID(s) {daemon['pids']}. Sampled footprint began at {footprints[0]:g} MiB, peaked at {max(footprints):g} MiB and ended at {footprints[-1]:g} MiB. In the ten-minute post-suite window, CPU had median {stats.median(cpu):g}% and maximum {max(cpu):g}% of one core. Collection errors: {errors}; invalid CPU samples: {invalid}.", "", "Five-second sampling can miss brief peaks, and memory inherits top's display rounding. The post-suite window precedes later signed-package checks. This observation does not prove that the earlier unreproduced daemon incident is fixed.", ""]
        summary[host] = report
    a.output.mkdir(parents=True, exist_ok=True)
    (a.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    (a.output / "RELEASE-0.5.1.md").write_text("\n".join(lines))
    print("Validated six full suites, four signed startup profiles and both daemon observations")


if __name__ == "__main__":
    main()
