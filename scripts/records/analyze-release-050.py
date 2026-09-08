"""Generate 0.5.0 release statistics from completed, immutable runs."""

import argparse, csv, datetime, gzip, json, math, statistics as st
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument("root", type=Path)
p.add_argument("--output", type=Path, required=True)
a = p.parse_args()
source = "1b0caab"


def observer_text(path):
    return (
        path.read_text()
        if path.exists()
        else gzip.decompress(
            path.with_suffix(path.suffix + ".gz").read_bytes()
        ).decode()
    )


def medians(path):
    d = {}
    for r in csv.DictReader(path.open()):
        d.setdefault(r["case"], []).append(float(r["ms"]))
    return {k: st.median(v) for k, v in d.items()}


def cv(v):
    return 100 * st.stdev(v) / st.mean(v) if st.mean(v) else 0


def unit(k):
    if k.startswith("memory-"):
        return "MiB"
    if k == "power-cpu-ms-per-s":
        return "CPU ms/s"
    if k == "power-energy-x10":
        return "energy ×10"
    if "wakeups" in k:
        return "wakeups/s"
    if k in ["net-http-latency", "net-http-p99", "net-dns"]:
        return "µs"
    if k == "net-connect-rate":
        return "connections/s"
    if k.startswith("net-"):
        return "Mbit/s"
    return "ms"


lines = [
    "# 0.5.0 release measurements",
    "",
    "The primary record for each host is its first valid complete host-share, guest-disk and amd64 suite, selected by execution order. Two further consecutive suites retain full-workload variation. Each timing row is the median of three timed repetitions. The harness attempts an untimed installation for each package manager and an untimed npm install to materialize read/metadata inputs. Warm-up and per-repetition setup exit statuses were not retained by this recording protocol. Ordinary measured-case stdout/stderr is retained and audited for setup diagnostics; the old memory sampler discarded successful-case output. That audit cannot retroactively verify ignored statuses. Each valid timing case requires three successful measured repetitions, whose first can still include remaining cold-cache cost. Those cases do not receive an additional per-case warm-up: the first timed read can be colder and is retained. Cold-start tests have an untimed round. Memory and power rows are single sampling windows.",
    "",
    "All runs use source `1b0caabf34fbe359f11cd47cc87ce45dc8583682`, with runtime code frozen at `66897086c3702b75d79cb6fcfed7736842b80780`. The runtime and guest fingerprints are checked throughout. Each fresh Lighter VM has eight vCPUs and a 128 GiB sparse disk, with 4 GiB guest RAM on M1 and 16 GiB on M5. Node 24.18.0, npm 11.16.0, pnpm 10.28.0 and Yarn 1.22.22 are pinned for native and container workloads. Identical prebuilt benchmark images are shared between runtimes and hosts.",
    "",
    "Each stage requires six consecutive host CPU observations at most 5% of aggregate machine capacity, ten seconds apart. Competing VMs are checked every second throughout; an unexpected VM invalidates the stage. CPU and filesystem-daemon observations remain available for judging interference during the workload. Neither daemon resets nor discarded slow repetitions improve the selected result.",
    "",
    "The five fresh storage runs below measure variation between per-run medians, using sample standard deviation divided by their arithmetic mean. This CV is descriptive for each workload and host; it is not a universal regression threshold. The separate ABBA comparison uses VMMs rebuilt from the published v0.4.1 source and the frozen 0.5.0 source with the same Rust toolchain, in order 0.4.1, 0.5.0, 0.5.0, 0.4.1. The baseline guest comes from the published 0.4.1 archive; the new guest is the frozen 0.5.0 payload. Both use the same pinned workload. Relative time change is the ratio of the two versions’ geometric means minus one. Positive means longer elapsed time. Two runs per version cannot establish a precise causal effect or justify calling every small difference a speedup.",
]
lines += [
    "",
    "Competitor preparation uses the same workload cases and pinned image archives. Its isolation helper additionally recognizes Lima 2.1 instance PID files and Apple VM helpers by their exact open instance disk, or by macOS responsibility identifying an explicitly allowed runtime executable. Docker Desktop shutdown uses its synchronous CLI stop command; the original broad process-name kill could also match the supervisor arguments. The corrected OrbStack and Docker Desktop process selectors match executable names rather than supervisor arguments. These harness changes and helper hashes accompany the records. Each competitor environment records the helper hash. Docker stores that report an OCI manifest digest instead of a configuration digest use a derived identity map from the same verified archive; no image layers or workload files change.",
]
lines += [
    "",
    "[Competitor methods and excluded cases](RELEASE-0.5.0-COMPETITORS.md) · [Repeatability](REPEATABILITY.md)",
    "",
    "",
]
summary = {}
for host in ["m1", "m5"]:
    root = a.root / host
    assert (root / "exit-code").read_text().strip() == "0"
    env = json.loads((root / "environment.json").read_text())
    prefix = f"050-{source}-{host}-a{env['attempt']}"
    summary[host] = {"environment": env, "full": {}, "variance": {}, "abba": {}}
    for stage in ["share", "guest", "amd64"]:
        runs = [medians(root / f"{prefix}-{n}-{stage}.csv") for n in [1, 2, 3]]
        lines += [
            "",
            f"## {host.upper()} — {stage}",
            "",
            "| Metric | Unit | Primary | Run 2 | Run 3 | Between-run CV |",
            "|---|---|---:|---:|---:|---:|",
        ]
        for case, value in runs[0].items():
            samples = [r[case] for r in runs]
            summary[host]["full"].setdefault(stage, {})[case] = {
                "medians": samples,
                "cv_percent": cv(samples),
            }
            lines.append(
                f"| {case} | {unit(case)} | {value:g} | {samples[1]:g} | {samples[2]:g} | {cv(samples):.2f}% |"
            )
    runs = [medians(root / f"{prefix}-variance-{n}.csv") for n in range(1, 6)]
    abba = [
        medians(root / f"{prefix}-abba-{n}-{v}.csv")
        for n, v in [(1, "041"), (2, "050"), (3, "050"), (4, "041")]
    ]

    def stamp(n, v):
        data = dict(
            line.split("=", 1)
            for line in (root / f"{prefix}-abba-{n}-{v}.tree").read_text().splitlines()
            if "=" in line
        )
        return {
            k: data[k]
            for k in [
                "runtime_source",
                "lighter-bench.sha256",
                "Image.sha256",
                "rootfs.ext4.sha256",
            ]
        }

    assert stamp(1, "041") == stamp(4, "041"), (
        "baseline artifact changed between ABBA runs"
    )
    assert stamp(2, "050") == stamp(3, "050"), "new artifact changed between ABBA runs"
    summary[host]["abba_artifacts"] = {"041": stamp(1, "041"), "050": stamp(2, "050")}
    lines += [
        "",
        f"## {host.upper()} — fresh storage variation and ABBA",
        "",
        "| Workload | Five same-build medians (ms) | CV | 0.4.1 geometric mean (ms) | 0.5.0 geometric mean (ms) | Relative time change |",
        "|---|---|---:|---:|---:|---:|",
    ]
    for case in runs[0]:
        values = [r[case] for r in runs]
        old = st.geometric_mean([abba[0][case], abba[3][case]])
        new = st.geometric_mean([abba[1][case], abba[2][case]])
        delta = 100 * (new / old - 1)
        summary[host]["variance"][case] = {"medians": values, "cv_percent": cv(values)}
        summary[host]["abba"][case] = {
            "ordered_medians": [r[case] for r in abba],
            "old_geomean": old,
            "new_geomean": new,
            "time_change_percent": delta,
        }
        lines.append(
            f"| {case} | "
            + ", ".join(f"{v:g}" for v in values)
            + f" | {cv(values):.2f}% | {old:.2f} | {new:.2f} | {delta:+.2f}% |"
        )
    rows = [json.loads(l) for l in observer_text(root / "daemon.jsonl").splitlines()]
    errors = [r for r in rows if "error" in r]
    valid = [r for r in rows if "pid" in r and "error" not in r]
    post = [r for r in valid if r["phase"] == "ten-minute observation"]
    assert post and valid
    duration = (
        datetime.datetime.fromisoformat(post[-1]["utc"])
        - datetime.datetime.fromisoformat(post[0]["utc"])
    ).total_seconds()
    assert duration >= 590, duration
    cpu_limit = {"m1": 800, "m5": 1800}[host]
    cpu_valid = [
        r
        for r in valid
        if math.isfinite(r["cpu_percent"]) and 0 <= r["cpu_percent"] <= cpu_limit
    ]
    cpu_invalid = [
        {k: r[k] for k in ["utc", "phase", "cpu_percent"]}
        for r in valid
        if not (math.isfinite(r["cpu_percent"]) and 0 <= r["cpu_percent"] <= cpu_limit)
    ]
    post_cpu = [r for r in cpu_valid if r["phase"] == "ten-minute observation"]
    assert post_cpu
    pids = sorted({r["pid"] for r in valid})
    peaks = [
        v["footprint_bytes"] / 1024**2
        for r in valid
        for v in r.get("vms", [])
        if v.get("footprint_bytes") is not None
    ]
    obs = {
        "pids": pids,
        "samples": len(valid),
        "errors": errors,
        "invalid_cpu_samples": cpu_invalid,
        "valid_cpu_samples": len(cpu_valid),
        "initial_mib": valid[0]["footprint_mib"],
        "peak_mib": max(r["footprint_mib"] for r in valid),
        "final_mib": valid[-1]["footprint_mib"],
        "peak_cpu_percent": max(r["cpu_percent"] for r in cpu_valid),
        "post_seconds": duration,
        "post_median_cpu_percent": st.median(r["cpu_percent"] for r in post_cpu),
        "post_max_cpu_percent": max(r["cpu_percent"] for r in post_cpu),
        "vm_peak_mib": max(peaks) if peaks else None,
    }
    summary[host]["observer"] = obs
    lines += [
        "",
        f"## {host.upper()} — filesystem-daemon observation",
        "",
        f"Observed fseventsd PIDs: {pids}. There were {len(valid)} memory observations, {len(cpu_valid)} physically valid CPU readings, {len(cpu_invalid)} invalid CPU readings and {len(errors)} collection errors. Invalid CPU values remain in the raw data and are shown as gaps in the graph; they do not enter CPU statistics. Five-second sampling can miss shorter peaks, and memory values inherit top’s display rounding. The daemon was not deliberately restarted.",
        "",
        f"Daemon footprint: initial {obs['initial_mib']:.2f} MiB, peak {obs['peak_mib']:.2f} MiB, final {obs['final_mib']:.2f} MiB. Peak sampled CPU was {obs['peak_cpu_percent']:.1f}% of one core. During the {duration:.0f}-second post-suite window, median CPU was {obs['post_median_cpu_percent']:.1f}% and maximum {obs['post_max_cpu_percent']:.1f}%.",
        "",
        f"The largest sampled Lighter task footprint was {obs['vm_peak_mib']:.1f} MiB. It includes host overhead and compressed-memory charges; configured guest RAM is not an absolute ceiling on the host process. Dedicated mixed-pressure records separately test memory at 8, 12 and 16 GiB.",
    ]
# The follow-up is a separately retained record, never a replacement primary.
for host in ["m1", "m5"]:
    follow = a.root / f"{host}-baab"
    if follow.exists():
        assert (follow / "exit-code").read_text().strip() == "0"
        env = json.loads((follow / "environment.json").read_text())
        records = json.loads((follow / "records.json").read_text())
        assert [r["version"] for r in records] == ["050", "041", "041", "050"]
        runs = [medians(follow / (r["label"] + ".csv")) for r in records]
        stamps = []
        for r in records:
            data = dict(
                line.split("=", 1)
                for line in (follow / (r["label"] + ".tree")).read_text().splitlines()
                if "=" in line
            )
            stamps.append(
                {
                    k: data[k]
                    for k in [
                        "runtime_source",
                        "lighter-bench.sha256",
                        "Image.sha256",
                        "rootfs.ext4.sha256",
                    ]
                }
            )
        assert stamps[0] == stamps[3] == summary[host]["abba_artifacts"]["050"]
        assert stamps[1] == stamps[2] == summary[host]["abba_artifacts"]["041"]
        summary[host]["baab"] = {}
        lines += [
            "",
            f"## {host.upper()} — reversed-order follow-up",
            "",
            env["reason"].rstrip(". ")
            + ". The follow-up uses the full storage workload in order 0.5.0, 0.4.1, 0.4.1, 0.5.0. Both comparisons are retained separately. It was selected after seeing the initial result, so it is not an independent preplanned replication.",
            "",
            "| Workload | Ordered medians (ms), 0.5/0.4/0.4/0.5 | Initial ABBA time change | Follow-up time change |",
            "|---|---|---:|---:|",
        ]
        for case in runs[0]:
            old = st.geometric_mean([runs[1][case], runs[2][case]])
            new = st.geometric_mean([runs[0][case], runs[3][case]])
            delta = 100 * (new / old - 1)
            summary[host]["baab"][case] = {
                "ordered_medians": [r[case] for r in runs],
                "old_geomean": old,
                "new_geomean": new,
                "time_change_percent": delta,
            }
            lines.append(
                "| "
                + case
                + " | "
                + ", ".join(f"{r[case]:g}" for r in runs)
                + f" | {summary[host]['abba'][case]['time_change_percent']:+.2f}% | {delta:+.2f}% |"
            )
lines += [
    "",
    "## Interpretation limits",
    "",
    "The M1 ripgrep case has a 56.86% CV across five fresh same-build medians (177, 267, 236, 294 and 643 ms). This protocol has poor precision for that workload on this host. Its apparent old/new differences do not establish a causal change. Other workloads have their own measured CVs; there is no universal five-percent noise threshold. Guest-disk tree-copy repetitions also vary substantially, so historical single-session differences are not treated as controlled regressions.",
]
if all("baab" in summary[h] for h in ["m1", "m5"]):
    lines += [
        "",
        "Several apparent changes reverse direction with order. On M1, ripgrep changes from {:+.2f}% in ABBA to {:+.2f}% in BAAB, and copy from {:+.2f}% to {:+.2f}%. Those initial slowdowns did not reproduce.".format(
            summary["m1"]["abba"]["ripgrep"]["time_change_percent"],
            summary["m1"]["baab"]["ripgrep"]["time_change_percent"],
            summary["m1"]["abba"]["copy-tree"]["time_change_percent"],
            summary["m1"]["baab"]["copy-tree"]["time_change_percent"],
        ),
    ]
    for host, case, title in [
        ("m1", "yarn-install", "M1 Yarn installation"),
        ("m5", "copy-tree", "M5 host-share tree copy"),
    ]:
        first = summary[host]["abba"][case]["time_change_percent"]
        second = summary[host]["baab"][case]["time_change_percent"]
        variation = summary[host]["variance"][case]["cv_percent"]
        lines += [
            "",
            f"{title} takes longer in both orders: {first:+.2f}% and {second:+.2f}%, against a five-fresh-run CV of {variation:.2f}%. This is a possible performance cost worth retaining, not evidence of no regression. The small number of runs and observed variation limit the precision and causal interpretation.",
        ]
    lines += [
        "",
        "M5 pnpm installation is shorter in both comparisons ({:+.2f}% and {:+.2f}%). Other small changes commonly switch direction. These workload-specific observations do not support a blanket speedup claim for 0.5.0.".format(
            summary["m5"]["abba"]["pnpm-install"]["time_change_percent"],
            summary["m5"]["baab"]["pnpm-install"]["time_change_percent"],
        ),
    ]
lines += [
    "",
    "These measurements do not establish a fix for the historical fseventsd incident if its original trigger did not recur. Raw samples and all attempted-run status remain part of the release record.",
]
a.output.mkdir(parents=True, exist_ok=True)
(a.output / "RELEASE-0.5.0.md").write_text("\n".join(lines) + "\n")
(a.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print("Generated report and summary", a.output)
