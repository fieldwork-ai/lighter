#!/usr/bin/env python3
"""Record three full suites, same-build variance and ABBA on a quiet Mac.

The first successful full suite is the primary release record. Failed runs are
retained and never selected. This runner deliberately stops on any invalid run.
"""

import argparse
from collections import Counter
import csv
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess as sp
import time

ROOT = Path(__file__).resolve().parents[2]
STORAGE = "npm-install pnpm-install yarn-install ripgrep find-walk copy-tree rm-rf"
SHARE = (
    STORAGE
    + " cpu-sha256 container-start watch-latency memory net-tcp-egress net-tcp-egress-r net-tcp-port net-tcp-port-r net-udp net-connect-rate net-http-latency net-dns power-idle boot"
)
AMD64 = "npm-install pnpm-install cpu-sha256 container-start"


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--machine", choices=["m1", "m5"], required=True)
    ap.add_argument("--source", required=True)
    ap.add_argument("--runtime-source", required=True)
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    os.chdir(ROOT)
    out = a.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    env = dict(
        os.environ,
        PATH=f"{Path.home()}/.orbstack/bin:/opt/homebrew/bin:{Path.home()}/.cargo/bin:"
        + os.environ["PATH"],
        BENCH_CPUS="8",
        BENCH_MEMORY_MIB="4096" if a.machine == "m1" else "16384",
        BENCH_DISK_GIB="128",
        LIGHTER_BENCH_KEEP_OUTPUT="1",
        LIGHTER_BENCH_SOURCE_SHA=a.runtime_source,
    )
    for name in [
        "LIGHTER_HOME",
        "LIGHTER_GUEST_DIR",
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "DOCKER_CONFIG",
        "LIGHTER_CMDLINE_EXTRA",
        "LIGHTER_VIRTIO_MEM",
        "LIGHTER_BENCH_BIN",
        "LIGHTER_BENCH_KERNEL",
        "LIGHTER_BENCH_GUEST_DIR",
        "LIGHTER_BENCH_ALLOW_NOISY",
    ]:
        env.pop(name, None)
    report = (ROOT / "benchmarks/RESULTS.md").read_bytes()
    phase = out / "phase"
    records = []
    monitor = None
    successful = False
    expected_artifacts = {}

    def digest(path):
        with path.open("rb") as source:
            return hashlib.file_digest(source, "sha256").hexdigest()

    def frozen():
        if sp.check_output(["git", "rev-parse", "HEAD"], text=True).strip() != a.source:
            raise RuntimeError("source HEAD changed during recording")
        sp.run(["git", "diff", "--quiet", "HEAD"], check=True)
        for path, expected in expected_artifacts.items():
            if digest(Path(path)) != expected:
                raise RuntimeError("runtime artifact changed: " + path)

    def run(label, cases, options=(), extra=None, target="lighter"):
        frozen()
        phase.write_text(label + "\n")
        result = ROOT / "benchmarks/results" / f"{label}.csv"
        if result.exists():
            raise RuntimeError("refusing to overwrite " + str(result))
        stage_env = dict(env, **(extra or {}))
        with (out / f"{label}.log").open("x") as log:
            command = [
                "python3",
                "benchmarks/guard.py",
                "--target",
                target,
                "--log",
                str(out / f"{label}-guard.jsonl"),
                "--timeout",
                "3600",
                "--quiet",
                "--",
                "bash",
                "benchmarks/run.sh",
                "--target",
                target,
                "--label",
                label,
                "--reps",
                "3",
                "--cases",
                cases,
                *options,
            ]
            p = sp.run(command, env=stage_env, stdout=log, stderr=sp.STDOUT)
        (ROOT / "benchmarks/RESULTS.md").write_bytes(report)
        if p.returncode:
            raise RuntimeError("invalid or failed stage " + label)
        rows = list(csv.DictReader(result.open()))
        counts = Counter(row["case"] for row in rows)
        for case in cases.split():
            names = {
                "memory": ["memory-peak", "memory-after-15s", "memory-after-60s"],
                "power-idle": ["power-cpu-ms-per-s", "power-wakeups-per-s"],
                "boot": ["boot-docker", "boot-first-container", "memory-idle"],
            }.get(case, [case])
            for name in names:
                expected = 1 if name.startswith(("memory-", "power-")) else 3
                if counts[name] != expected:
                    raise RuntimeError(
                        f"{label}: {name} has {counts[name]}, expected {expected}"
                    )
        if any(float(row["ms"]) < 0 for row in rows):
            raise RuntimeError("negative measurement")
        if target == "lighter":
            boot = ROOT / "benchmarks/results/lighter-boot.log"
            text = boot.read_text(errors="replace")
            import re

            if re.search(
                r"rcu.*stall|soft lockup|hard LOCKUP|BUG:|kernel panic|Oops:",
                text,
                re.I,
            ):
                raise RuntimeError("kernel fault in " + label)
            shutil.copyfile(boot, out / f"{label}-boot.log")
        shutil.copyfile(result, out / result.name)
        shutil.copyfile(
            result.with_suffix(".tree"), out / result.with_suffix(".tree").name
        )
        records.append(
            dict(
                label=label,
                target=target,
                cases=cases,
                options=options,
                sha256=digest(result),
            )
        )
        (out / "records.json").write_text(json.dumps(records, indent=2) + "\n")
        frozen()
        print("PASS", label, flush=True)

    try:
        frozen()
        # Build once before quiet-host checks; no runtime source mutation after here.
        with (out / "build.log").open("w") as log:
            sp.run(
                [
                    "cargo",
                    "build",
                    "--release",
                    "--example",
                    "lighter-bench",
                    "-p",
                    "lighter-vmm",
                ],
                env=env,
                stdout=log,
                stderr=sp.STDOUT,
                check=True,
            )
            sp.run(
                [
                    "scripts/sign.sh",
                    "target/release/examples/lighter-bench",
                    "target/release/lighter",
                ],
                env=env,
                stdout=log,
                stderr=sp.STDOUT,
                check=True,
            )
        for name in [
            "guest/out/Image",
            "guest/out/rootfs.ext4",
            "target/release/examples/lighter-bench",
            "target/release/lighter",
        ]:
            expected_artifacts[str(ROOT / name)] = digest(ROOT / name)
        (out / "environment.json").write_text(
            json.dumps(
                dict(
                    source=a.source,
                    runtime_source=a.runtime_source,
                    artifacts=expected_artifacts,
                    profile={
                        key: env[key]
                        for key in ["BENCH_CPUS", "BENCH_MEMORY_MIB", "BENCH_DISK_GIB"]
                    },
                    system=sp.check_output(["sw_vers"], text=True),
                    baseline_source=sp.check_output(
                        ["git", "rev-parse", "v0.4.1"], text=True
                    ).strip(),
                    primary="first valid full suite",
                    quiet="six samples 10s apart, aggregate machine CPU <=5%; cap 15m",
                    repetitions=3,
                ),
                indent=2,
            )
            + "\n"
        )
        phase.write_text("baseline\n")
        monitor = sp.Popen(
            ["python3", "scripts/records/monitor-host.py", str(out)], env=env
        )
        prefix = f"050-{a.source[:7]}-{a.machine}"
        for suite in range(1, 4):
            for stage, cases, options in [
                ("share", SHARE, ()),
                ("guest", STORAGE, ("--where", "guest")),
                ("amd64", AMD64, ("--where", "guest", "--arch", "amd64")),
            ]:
                run(f"{prefix}-{suite}-{stage}", cases, options)
        phase.write_text("ten-minute observation\n")
        # Continue enforcing VM exclusivity throughout the observation window.
        sp.run(
            [
                "python3",
                "benchmarks/guard.py",
                "--target",
                "native",
                "--log",
                str(out / "observation-guard.jsonl"),
                "--timeout",
                "660",
                "--",
                "sleep",
                "600",
            ],
            env=env,
            check=True,
        )
        for index in range(1, 6):
            run(f"{prefix}-variance-{index}", STORAGE)
        baseline = ROOT / ".logs/050/targets/041/release/examples/lighter-bench"
        baseline_guest = (
            ROOT / ".logs/050/baseline-artifact/lighter-0.4.1/share/lighter"
        )
        baseline_source = sp.check_output(
            ["git", "rev-parse", "v0.4.1"], text=True
        ).strip()
        for index, version in enumerate(["041", "050", "050", "041"], 1):
            extra = (
                dict(
                    LIGHTER_BENCH_BIN=str(baseline),
                    LIGHTER_BENCH_GUEST_DIR=str(baseline_guest),
                    LIGHTER_BENCH_SOURCE_SHA=baseline_source,
                )
                if version == "041"
                else None
            )
            run(f"{prefix}-abba-{index}-{version}", STORAGE, extra=extra)
        run(f"{prefix}-native", STORAGE + " cpu-sha256 watch-latency", target="native")
        phase.write_text("complete\n")
        successful = True
    finally:
        (ROOT / "benchmarks/RESULTS.md").write_bytes(report)
        if monitor:
            monitor.terminate()
            try:
                monitor.wait(10)
            except sp.TimeoutExpired:
                monitor.kill()
                monitor.wait()
        (out / "exit-code").write_text("0\n" if successful else "1\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
