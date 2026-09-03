#!/usr/bin/env python3
"""Turns the benchmark CSVs into a report.

Nothing here is hand-editable, which is the point: a number in the report that
no CSV supports cannot exist. The median is reported rather than the mean or
the best, because the best run is a claim about the machine being idle and the
mean is dragged around by a single scheduling hiccup.
"""

import csv
import pathlib
import platform
import statistics
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
RESULTS = HERE / "results"

# The order they are reported in, and what each one is actually measuring.
CASES = [
    ("npm-install", "npm ci of a pinned tree", "lower is better"),
    ("pnpm-install", "pnpm install --frozen-lockfile", "lower is better"),
    ("yarn-install", "yarn install --frozen-lockfile", "lower is better"),
    ("ripgrep", "reading every file in node_modules", "lower is better"),
    ("find-walk", "metadata-only walk of node_modules", "lower is better"),
    ("copy-tree", "copying node_modules within the share", "lower is better"),
    ("rm-rf", "rm -rf of a node_modules tree", "lower is better"),
    ("watch-latency", "host change to guest visibility, round trip", "lower is better"),
]

# The runtime's cost to the Mac, in MiB rather than milliseconds: the
# physical footprint of its processes as Activity Monitor accounts it, settled
# before an install, at the peak through one, and fifteen and sixty seconds
# after it ends. Reported in their own table; `native` has no runtime.
MEMORY_CASES = [
    ("memory-settled", "settled, before an install"),
    ("memory-peak", "peak through an npm install"),
    ("memory-after-15s", "15 s after it ends"),
    ("memory-after-60s", "60 s after it ends"),
]

# The network, in the unit each path is naturally read in. Throughput cases
# are Mbit/s and higher is better; the rest are rates and latencies. `native`
# is the Mac over loopback where that means something, and blank where it
# does not (there is no "egress" from the Mac to itself).
NETWORK_CASES = [
    ("net-tcp-egress", "TCP, container to the Mac", "Mbit/s", "higher"),
    ("net-tcp-egress-r", "TCP, the Mac to a container", "Mbit/s", "higher"),
    ("net-tcp-port", "TCP into a published port", "Mbit/s", "higher"),
    ("net-tcp-port-r", "TCP out of a published port", "Mbit/s", "higher"),
    ("net-udp", "UDP, container to the Mac", "Mbit/s", "higher"),
    ("net-connect-rate", "connects to a published port", "per second", "higher"),
    ("net-http-latency", "GET on a published port, median", "µs", "lower"),
    ("net-http-p99", "GET on a published port, p99", "µs", "lower"),
    ("net-dns", "DNS lookup from a container, median", "µs", "lower"),
]

# What an idle runtime costs, sampled by `top` over a quiet minute: CPU as
# milliseconds per second, idle wakeups per second, and top's energy-impact
# figure (stored times ten, shown with one decimal). Lower is better for all.
POWER_CASES = [
    ("power-cpu-ms-per-s", "CPU, ms per second", 1),
    ("power-wakeups-per-s", "wakeups per second", 1),
    ("power-pkg-idle-wakeups-per-s", "package-idle wakeups per second", 1),
    ("power-energy-x10", "energy impact (top)", 10),
]

# Cases the native ratio says nothing useful about. `watch-latency` is the
# only one: on the Mac a file is visible the moment it is written, so the
# reference is nearly zero and every ratio against it is a division by noise —
# a container seeing a change in 5ms would read as "20% of native", which
# sounds like a failure and is in fact a millisecond.
UNRATIOED = {"watch-latency"}

# `native` is the reference: the same command on the Mac's own disk.
REFERENCE = "native"


def load(target, results=RESULTS):
    path = results / f"{target}.csv"
    if not path.exists():
        return {}
    runs = {}
    with path.open() as handle:
        for row in csv.DictReader(handle):
            if row["ms"] == "timeout":
                # Killed by the runner's per-case limit. Not a measurement,
                # and not silently a gap either.
                print(f"note: {target} {row['case']} rep {row['rep']} timed out", file=sys.stderr)
                continue
            runs.setdefault(row["case"], []).append(int(row["ms"]))
    return {case: statistics.median(values) for case, values in runs.items() if values}


def tool_version(*command):
    try:
        out = subprocess.run(command, capture_output=True, text=True, timeout=10)
        return (out.stdout or out.stderr).strip().splitlines()[0]
    except Exception:
        return "not installed"


# The runtimes the report is a comparison *of*.
RUNTIMES = ("native", "lighter", "orbstack", "colima", "docker-desktop")


def describe_this_machine():
    """What `scripts/describe-machine.sh` says about the machine running this."""
    try:
        out = subprocess.run(
            [str(HERE.parent / "scripts" / "describe-machine.sh")],
            capture_output=True,
            text=True,
            timeout=20,
        )
        return out.stdout.strip() or f"{platform.machine()}, macOS {platform.mac_ver()[0]}"
    except Exception:
        return f"{platform.machine()}, macOS {platform.mac_ver()[0]}"


def machines():
    """Every result set: this machine's in `results/`, and one directory per
    other machine under `results/machines/`, each with the `machine.txt` its
    own `scripts/describe-machine.sh` wrote."""
    sets = [(RESULTS, describe_this_machine(), True)]
    for entry in sorted((RESULTS / "machines").glob("*/")):
        note = entry / "machine.txt"
        if note.exists() and any(entry.glob("*.csv")):
            sets.append((entry, note.read_text().strip(), False))
    return sets


def main():
    lines = [
        "# Benchmark results",
        "",
        "Generated by `benchmarks/report.py` from the CSVs in `benchmarks/results/`.",
        "Do not edit: regenerate it.",
        "",
        "Every target ran the same case scripts against the same fixture on the same",
        "machine, with caches warmed by an untimed run first. The figure is the median",
        "of the repetitions. Each machine is its own section; a number is only ever",
        "compared with another from the same machine and the same session.",
        "",
        f"- Node: {tool_version('node', '--version')}",
        f"- npm: {tool_version('npm', '--version')}",
        f"- ripgrep: {tool_version('rg', '--version')}",
        f"- pnpm: {tool_version('pnpm', '--version')}",
        f"- yarn: {tool_version('yarn', '--version')}",
        "",
    ]
    rendered = 0
    for results, description, primary in machines():
        section = render(results, description, primary)
        if section:
            lines += section
            rendered += 1
    if not rendered:
        print("no results yet; run benchmarks/run.sh --target <t>", file=sys.stderr)
        return 1

    lines += ["## What each case does", ""]
    for case, description, direction in CASES:
        lines.append(f"- **{case}** — {description} ({direction})")
    lines.append("")

    (HERE / "RESULTS.md").write_text("\n".join(lines))
    print(f"wrote {HERE / 'RESULTS.md'}")
    return 0


def render(results, description, _primary):
    """One machine's tables."""
    # Only the runtimes are reported. Everything else in the directory is a
    # labelled run — a sweep, a diagnosis, a repetition kept for the record —
    # and stays as a CSV anyone can read; a hundred such columns in one table
    # was a table nobody could.
    found = sorted(p.stem for p in results.glob("*.csv"))
    targets = [t for t in RUNTIMES if t in found]
    if not targets:
        return []
    measured = {target: load(target, results) for target in targets}
    reference = measured.get(REFERENCE, {})

    # Two places a container's files can live, side by side: the host share
    # (a bind mount of the Mac's filesystem) and the runtime's own disk (a
    # named volume, where a container's writable layer and its data volumes
    # live). `native` is the same command on the Mac's own disk, and is the
    # one reference for both: the share is asked how close it comes to the
    # Mac's disk from inside a VM, the own disk how a Linux filesystem in a
    # VM compares with the Mac's.
    columns = []
    if REFERENCE in targets:
        columns.append((REFERENCE, reference))
    for t in targets:
        if t != REFERENCE:
            columns.append((f"{t} (share)", measured[t]))
    for t in RUNTIMES:
        if (results / f"{t}-guest.csv").exists():
            columns.append((f"{t} (own disk)", load(f"{t}-guest", results)))

    lines = [
        f"## {description}",
        "",
        "### Wall time, milliseconds",
        "",
        "`native` is the command on the Mac's own disk. `share` is a bind mount of",
        "the same tree into the container; `own disk` is a named volume, the",
        "runtime's own filesystem inside the VM, where a container's writable layer",
        "and its data volumes live.",
        "",
        "| case | " + " | ".join(name for name, _ in columns) + " |",
        "|" + "---|" * (len(columns) + 1),
    ]
    for case, _, _ in CASES:
        if not any(case in values for _, values in columns):
            continue
        cells = []
        for _, values in columns:
            value = values.get(case)
            cells.append("—" if value is None else f"{int(value)}")
        lines.append(f"| {case} | " + " | ".join(cells) + " |")
    lines.append("")

    # The share columns only: the reading is of the runtime, and the own-disk
    # leg of the same runtime measures the same processes again.
    memory_columns = [
        (name, values)
        for name, values in columns
        if name != REFERENCE
        and not name.endswith("(own disk)")
        and any(case in values for case, _ in MEMORY_CASES)
    ]
    if memory_columns:
        lines += [
            "### What the runtime costs the Mac, MiB",
            "",
            "The physical footprint of the runtime's own processes, as Activity Monitor",
            "accounts it — which reads high for any Hypervisor.framework guest, and the",
            "same way for every runtime here. Lower is better; the last two columns are",
            "what a runtime gives back on its own after the work ends.",
            "",
            "| reading | " + " | ".join(name for name, _ in memory_columns) + " |",
            "|" + "---|" * (len(memory_columns) + 1),
        ]
        for case, label in MEMORY_CASES:
            cells = []
            for _, values in memory_columns:
                value = values.get(case)
                cells.append("—" if value is None else f"{int(value)}")
            lines.append(f"| {label} | " + " | ".join(cells) + " |")
        lines.append("")

    # The share columns plus native: the network does not care where the
    # fixture lives, and the own-disk leg measures the same link again.
    network_columns = [
        (name, values)
        for name, values in columns
        if not name.endswith("(own disk)")
        and any(case in values for case, _, _, _ in NETWORK_CASES)
    ]
    if network_columns:
        lines += [
            "### The network",
            "",
            "iperf3 between a container and the Mac in both directions, on the",
            "path a container sees (its egress to the Mac's LAN address) and on the",
            "path the Mac sees (a published port on localhost); then connection",
            "setup, request latency on a kept-alive connection, and DNS from inside",
            "a container. `native` is the Mac over loopback where that means anything.",
            "",
            "| case | unit | " + " | ".join(name for name, _ in network_columns) + " |",
            "|" + "---|" * (len(network_columns) + 2),
        ]
        for case, label, unit, _ in NETWORK_CASES:
            if not any(case in values for _, values in network_columns):
                continue
            cells = []
            for _, values in network_columns:
                value = values.get(case)
                cells.append("—" if value is None else f"{int(value)}")
            lines.append(f"| {label} | {unit} | " + " | ".join(cells) + " |")
        lines.append("")

    power_columns = [
        (name, values)
        for name, values in columns
        if name != REFERENCE
        and not name.endswith("(own disk)")
        and any(case in values for case, _, _ in POWER_CASES)
    ]
    if power_columns:
        lines += [
            "### What an idle runtime costs",
            "",
            "After a quiet minute, a minute of powermetrics samples over the",
            "runtime's processes (top's columns where powermetrics is not allowed):",
            "CPU as milliseconds of core per second, wakeups per second, and the",
            "wakeups that pull the package out of idle, which are the battery's.",
            "Lower is better throughout.",
            "",
            "| reading | " + " | ".join(name for name, _ in power_columns) + " |",
            "|" + "---|" * (len(power_columns) + 1),
        ]
        for case, label, scale in POWER_CASES:
            cells = []
            for _, values in power_columns:
                value = values.get(case)
                if value is None:
                    cells.append("—")
                elif scale == 1:
                    cells.append(f"{int(value)}")
                else:
                    cells.append(f"{value / scale:.1f}")
            lines.append(f"| {label} | " + " | ".join(cells) + " |")
        lines.append("")

    if reference:
        ratioed = [(name, values) for name, values in columns if name != REFERENCE]
        lines += [
            f"### As a fraction of `{REFERENCE}`",
            "",
            "100% is the Mac's own disk. For the share, higher is the boundary costing",
            "less; for the own disk, more than 100% is a filesystem inside the VM",
            "outrunning the Mac's. `watch-latency` is left out: it is a latency against",
            "a reference of about a millisecond, so the ratio is a division by noise —",
            "read it from the table above in milliseconds.",
            "",
            "| case | " + " | ".join(name for name, _ in ratioed) + " |",
            "|" + "---|" * (len(ratioed) + 1),
        ]
        for case, _, _ in CASES:
            base = reference.get(case)
            if not base or case in UNRATIOED:
                continue
            cells = []
            for _, values in ratioed:
                value = values.get(case)
                cells.append("—" if not value else f"{base / value * 100:.0f}%")
            lines.append(f"| {case} | " + " | ".join(cells) + " |")
        lines.append("")

    return lines


if __name__ == "__main__":
    sys.exit(main())
