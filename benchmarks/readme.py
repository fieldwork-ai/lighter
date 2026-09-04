"""Writes the README's benchmark section from the CSVs.

The README's tables are derived, never typed: `python3 benchmarks/readme.py
--write` replaces everything between `## Benchmarks` and the next top-level
heading with what the CSVs support, the same medians `report.py` reports.
The prose around the tables lives here too, so a number in the README that
no CSV supports cannot exist — the reason `report.py` was written the same way.
"""

import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import report  # noqa: E402

README = report.HERE.parent / "README.md"

# Which results directory is which machine, and the heading it gets.
MACHINES = [
    (report.RESULTS, "Apple M5 Pro (18 cores, 48 GB RAM)"),
    (report.RESULTS / "machines" / "m1", "Apple M1 (8 cores, 8 GB RAM)"),
]

STORAGE = [
    ("npm-install", "`npm ci`"),
    ("pnpm-install", "`pnpm install`"),
    ("yarn-install", "`yarn install`"),
    ("ripgrep", "`ripgrep` (file read)"),
    ("find-walk", "`find` (metadata walk)"),
    ("copy-tree", "`cp -a node_modules`"),
    ("rm-rf", "`rm -rf node_modules`"),
    ("watch-latency", "Host file edit -> container"),
]
RUNTIMES = [
    ("lighter", "lighter"),
    ("orbstack", "OrbStack"),
    ("colima", "Colima"),
    ("docker-desktop", "Docker Desktop"),
]

INTRO = """Measured on clean machines against a 1,232-package `package.json` fixture (`benchmarks/`). Each figure is the median of three timed repetitions, following an untimed warm-up run. Numbers are reported as absolute time and as a percentage of native APFS on the same machine (higher means faster). The first table is the runtime's own disk, where a container's writable layer and its volumes live; the second is a host share, the Mac's directory bind-mounted into the container. Bold marks the fastest runtime in each row; a dash is a case the runtime could not complete.

OrbStack, Colima and Docker Desktop were measured on the same machines in the same sessions."""

MEMORY_INTRO = """The physical footprint of the runtime's own processes, which is the "Memory" column in Activity Monitor: settled before an `npm ci`, at its peak during one, and 15 and 60 seconds after it ends with nothing running. Lower is better throughout."""

NETWORK_INTRO = """iperf3 between a container and the Mac in both directions, on the path a container sees (its egress to the Mac's LAN address) and on the path the Mac sees (a published port on localhost); then connection setup, request latency on a kept-alive connection, and DNS from inside a container. Bold marks the best runtime in each row."""

POWER_INTRO = """After a quiet minute, a minute of powermetrics samples over the runtime's processes: CPU as milliseconds of core per second, and wakeups per second. Lower is better."""


def ms(value):
    if value is None:
        return "—"
    if value >= 1000:
        return f"{value / 1000:.2f} s"
    return f"{int(value)} ms"


def storage_table(results, where):
    """The own-disk or host-share table: native, then each runtime, each
    figure with its fraction of native."""
    suffix = "" if where == "share" else "-guest"
    native = report.load("native", results)
    runtimes = [(key, name, report.load(f"{key}{suffix}", results)) for key, name in RUNTIMES]
    runtimes = [(key, name, values) for key, name, values in runtimes if values]
    if not runtimes:
        return ""
    head = "| Workload (" + ("host share" if where == "share" else "own disk") + ") | native APFS"
    head += "".join(f" | {name}" for _, name, _ in runtimes) + " |"
    lines = [head, "|---" * (2 + len(runtimes)) + "|"]
    for case, label in STORAGE:
        if where != "share" and case == "watch-latency":
            continue
        values = [v.get(case) for _, _, v in runtimes]
        if all(v is None for v in values) and native.get(case) is None:
            continue
        present = [v for v in values if v is not None]
        best = min(present) if present else None
        cells = [label, ms(native.get(case))]
        for value in values:
            if value is None:
                cells.append("—")
                continue
            cell = ms(value)
            if value == best and len(present) > 1:
                cell = f"**{cell}**"
            if native.get(case) and case not in report.UNRATIOED:
                cell += f" ({native[case] / value * 100:.0f}%)"
            cells.append(cell)
        lines.append("| " + " | ".join(cells) + " |")
    return "\n".join(lines)


def memory_table(results):
    runtimes = [(name, report.load(key, results)) for key, name in RUNTIMES]
    runtimes = [(name, v) for name, v in runtimes if any(c in v for c, _ in report.MEMORY_CASES)]
    if not runtimes:
        return ""
    lines = ["| Reading" + "".join(f" | {name}" for name, _ in runtimes) + " |", "|---" * (1 + len(runtimes)) + "|"]
    for case, label in report.MEMORY_CASES:
        values = [v.get(case) for _, v in runtimes]
        present = [v for v in values if v is not None]
        if not present:
            continue
        best = min(present)
        cells = [label[0].upper() + label[1:]]
        for value in values:
            cell = "—" if value is None else f"{int(value)} MiB"
            if value == best and len(present) > 1:
                cell = f"**{cell}**"
            cells.append(cell)
        lines.append("| " + " | ".join(cells) + " |")
    return "\n".join(lines)


def network_table(results):
    native = report.load("native", results)
    runtimes = [(name, report.load(key, results)) for key, name in RUNTIMES]
    runtimes = [(name, v) for name, v in runtimes if any(c in v for c, _, _, _, _ in report.NETWORK_CASES)]
    if not runtimes:
        return ""
    lines = ["| Case | unit | native" + "".join(f" | {name}" for name, _ in runtimes) + " |", "|---" * (3 + len(runtimes)) + "|"]
    for case, label, unit, direction, divisor in report.NETWORK_CASES:
        values = [v.get(case) for _, v in runtimes]
        present = [v for v in values if v is not None]
        if not present:
            continue
        best = max(present) if direction == "higher" else min(present)
        cells = [label, unit, report.network_cell(native.get(case), divisor)]
        for value in values:
            cell = report.network_cell(value, divisor)
            if value == best and len(present) > 1:
                cell = f"**{cell}**"
            cells.append(cell)
        lines.append("| " + " | ".join(cells) + " |")
    return "\n".join(lines)


def power_table(results):
    runtimes = [(name, report.load(key, results)) for key, name in RUNTIMES]
    runtimes = [(name, v) for name, v in runtimes if any(c in v for c, _, _ in report.POWER_CASES)]
    if not runtimes:
        return ""
    lines = ["| Reading" + "".join(f" | {name}" for name, _ in runtimes) + " |", "|---" * (1 + len(runtimes)) + "|"]
    for case, label, scale in report.POWER_CASES[:2]:
        values = [v.get(case) for _, v in runtimes]
        present = [v for v in values if v is not None]
        if not present:
            continue
        best = min(present)
        cells = [label[0].upper() + label[1:]]
        for value in values:
            cell = "—" if value is None else f"{value / scale:.0f}"
            if value == best and len(present) > 1:
                cell = f"**{cell}**"
            cells.append(cell)
        lines.append("| " + " | ".join(cells) + " |")
    return "\n".join(lines)


def section():
    out = ["## Benchmarks", "", INTRO, ""]
    for results, heading in MACHINES:
        if not results.exists() or not any(results.glob("*.csv")):
            continue
        out += [f"### {heading}", ""]
        guest = storage_table(results, "guest")
        if guest:
            out += [guest, ""]
        share = storage_table(results, "share")
        if share:
            out += [share, ""]
        memory = memory_table(results)
        if memory:
            out += ["#### What the runtime costs the Mac", "", MEMORY_INTRO, "", memory, ""]
        network = network_table(results)
        if network:
            out += ["#### The network", "", NETWORK_INTRO, "", network, ""]
        power = power_table(results)
        if power:
            out += ["#### What an idle runtime costs", "", POWER_INTRO, "", power, ""]
    out += ["`benchmarks/RESULTS.md` contains the full logs, individual repetition timings, and methodology.", ""]
    return "\n".join(out)


def main():
    text = section()
    if "--write" in sys.argv:
        readme = README.read_text()
        pattern = re.compile(r"## Benchmarks\n.*?(?=\n## )", re.S)
        if not pattern.search(readme):
            sys.exit("README.md has no `## Benchmarks` section to replace")
        README.write_text(pattern.sub(lambda _: text.rstrip("\n"), readme, count=1))
        print(f"wrote {README}")
    else:
        print(text)


if __name__ == "__main__":
    main()
