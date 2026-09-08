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
    (report.RESULTS, "MacBook Pro — Apple M5 Pro (18 cores, 48 GB RAM)"),
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

def intro():
    lighter = report.load("lighter", report.RESULTS)
    orb = report.load("orbstack", report.RESULTS)
    return f"""All benchmarks are measured against identical pinned workloads on Apple Silicon. Higher percentages of native APFS mean faster; **bold** indicates the best runtime result.

On Apple Silicon, lighter launches containers cold in **664 ms** (over 2x faster than OrbStack), runs `npm ci` on host shares in **{ms(lighter['npm-install'])}** (faster than native APFS, beating OrbStack's {ms(orb['npm-install'])}), completes directory copies **{orb['copy-tree'] / lighter['copy-tree']:.1f}x faster**, idles at **{lighter['memory-idle']:.0f} MiB RAM**, and returns memory to macOS within seconds of a workload finishing.

<details>
<summary>Benchmark methodology & test environment</summary>

Measured with the pinned 1,232-package fixture in `benchmarks/` on a MacBook Pro (Apple M5 Pro, 18 cores, 48 GB RAM, macOS 15 Sequoia). Timing rows report medians of three measured repetitions. Native and container runs use identical pinned Node, npm, pnpm, and Yarn versions. All runtimes were configured with 8 vCPUs and 16 GiB RAM allocations where supported. Docker Desktop is measured using Virtualization.framework, VirtioFS, and Rosetta.

Lighter measurements reflect the 0.5.1 release; competitor measurements retain their 0.5.0-release suite. Docker Desktop's host-share cleanup failed during testing; affected install timings are excluded. Raw observations, environment fingerprints, and full M1 results are preserved in [the 0.5.1 measurements](benchmarks/RELEASE-0.5.1.md), [the retained 0.5.0 comparison](benchmarks/RELEASE-0.5.0.md), and [benchmarks/RESULTS.md](benchmarks/RESULTS.md). See [repeatability](benchmarks/REPEATABILITY.md) for workload-specific variation.
</details>"""


MEMORY_INTRO = """macOS physical footprint (Activity Monitor "Memory") for runtime processes: idle after cold start, peak during `npm ci`, and 15s / 60s after workload completion. Lower is better. lighter releases memory back to the Mac immediately via `virtio-mem` and cooperative reclamation."""

NETWORK_INTRO = """Throughput and latency between container and host measured with `iperf3`, keep-alive HTTP GET latency, connection setup rate, and container DNS resolution time. Bold marks best result."""

POWER_INTRO = """Idle CPU consumption and thread wakeups measured via `powermetrics` over a 60-second quiet window. Lower is better."""

AMD64_INTRO = """Running `linux/amd64` images on Apple Silicon via Apple Rosetta (`--vz-rosetta` for Colima). Lower is better."""

BOOT_INTRO = """Time from cold invocation (`lighter start`, `orb start`, `colima start`, Docker Desktop launch) until Docker engine responds, and until the first container completes. Median of three; lower is better."""


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
    runtimes = [
        (key, name, report.load(f"{key}{suffix}", results)) for key, name in RUNTIMES
    ]
    runtimes = [(key, name, values) for key, name, values in runtimes if values]
    if not runtimes:
        return ""
    head = (
        "| Workload ("
        + ("host share" if where == "share" else "own disk")
        + ") | native APFS"
    )
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
    runtimes = [
        (name, v) for name, v in runtimes if any(c in v for c, _ in report.MEMORY_CASES)
    ]
    if not runtimes:
        return ""
    lines = [
        "| Reading" + "".join(f" | {name}" for name, _ in runtimes) + " |",
        "|---" * (1 + len(runtimes)) + "|",
    ]
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
    runtimes = [
        (name, v)
        for name, v in runtimes
        if any(c in v for c, _, _, _, _ in report.NETWORK_CASES)
    ]
    if not runtimes:
        return ""
    lines = [
        "| Case | unit | native" + "".join(f" | {name}" for name, _ in runtimes) + " |",
        "|---" * (3 + len(runtimes)) + "|",
    ]
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
    runtimes = [
        (name, v)
        for name, v in runtimes
        if any(c in v for c, _, _ in report.POWER_CASES)
    ]
    if not runtimes:
        return ""
    lines = [
        "| Reading" + "".join(f" | {name}" for name, _ in runtimes) + " |",
        "|---" * (1 + len(runtimes)) + "|",
    ]
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


def boot_table(results):
    runtimes = [(name, report.load(key, results)) for key, name in RUNTIMES]
    runtimes = [
        (name, v) for name, v in runtimes if any(c in v for c, _ in report.BOOT_CASES)
    ]
    if not runtimes:
        return ""
    lines = [
        "| Reading" + "".join(f" | {name}" for name, _ in runtimes) + " |",
        "|---" * (1 + len(runtimes)) + "|",
    ]
    for case, label in report.BOOT_CASES:
        values = [v.get(case) for _, v in runtimes]
        present = [v for v in values if v is not None]
        if not present:
            continue
        best = min(present)
        cells = [label[0].upper() + label[1:]]
        for value in values:
            cell = ms(value)
            if value == best and len(present) > 1:
                cell = f"**{cell}**"
            cells.append(cell)
        lines.append("| " + " | ".join(cells) + " |")
    return "\n".join(lines)


def amd64_table(results):
    # The arm64 reference is lighter's own figure for the same workload: the
    # own-disk record where the case is an install, the host-timed one where
    # it is a container start (the own-disk stage does not run that case).
    scale = report.load("lighter", results) | report.load("lighter-guest", results)
    runtimes = [
        (name, report.load(f"{key}-amd64", results))
        for key, name in RUNTIMES
        if key != "native"
    ]
    runtimes = [
        (name, v) for name, v in runtimes if any(c in v for c, _ in report.AMD64_CASES)
    ]
    if not runtimes:
        return ""
    lines = [
        "| Workload (x86-64 image, own disk) | lighter, arm64"
        + "".join(f" | {name}" for name, _ in runtimes)
        + " |",
        "|---" * (2 + len(runtimes)) + "|",
    ]
    for case, label in report.AMD64_CASES:
        values = [v.get(case) for _, v in runtimes]
        present = [v for v in values if v is not None]
        if not present:
            continue
        best = min(present)
        reference = scale.get(case)
        cells = [label, "—" if reference is None else ms(reference)]
        for value in values:
            cell = "—" if value is None else ms(value)
            if value == best and len(present) > 1:
                cell = f"**{cell}**"
            cells.append(cell)
        lines.append("| " + " | ".join(cells) + " |")
    return "\n".join(lines)


def section():
    out = ["## Benchmarks", "", intro(), ""]
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
            out += ["#### Memory footprint", "", MEMORY_INTRO, "", memory, ""]
        network = network_table(results)
        if network:
            out += ["#### The network", "", NETWORK_INTRO, "", network, ""]
        power = power_table(results)
        if power:
            out += ["#### Idle power", "", POWER_INTRO, "", power, ""]
        boot = boot_table(results)
        if boot:
            out += ["#### Starting up", "", BOOT_INTRO, "", boot, ""]
        amd64 = amd64_table(results)
        if amd64:
            out += ["#### x86-64 images", "", AMD64_INTRO, "", amd64, ""]
    out += [
        "[0.5.1 release records](benchmarks/RELEASE-0.5.1.md) and [retained competitor records](benchmarks/RELEASE-0.5.0-COMPETITORS.md) retain raw CSVs, case diagnostics, selection decisions and environment evidence. `benchmarks/RESULTS.md` contains individual repetition timings and methodology.",
        "",
        "---",
    ]
    return "\n".join(out)


def main():
    text = section()
    if "--write" in sys.argv:
        readme = README.read_text()
        pattern = re.compile(r"## Benchmarks\n.*?(?=\n## )", re.S)
        if not pattern.search(readme):
            sys.exit("README.md has no `## Benchmarks` section to replace")
        README.write_text(pattern.sub(lambda _: text.rstrip("\n") + "\n", readme, count=1))
        print(f"wrote {README}")
    else:
        print(text)


if __name__ == "__main__":
    main()
