#!/usr/bin/env python3
"""Writes the README's benchmark section from a compare.sh record.

The README's tables are derived, never typed: `python3 benchmarks/readme.py
--write` replaces everything from `## Benchmarks` to the next `## ` heading
with what the newest record's CSVs support (medians, a case failed if any
repetition did), so a number in the README that no CSV supports cannot
exist. The prose around the tables lives here too. Without `--write` it
prints the section.

    benchmarks/readme.py [--write] [<record dir>]

The record is the newest `benchmarks/results/machines/m5/compare-*` unless
one is named; its own README carries the method and every caveat.
"""

import csv
import re
import statistics
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
README = HERE.parent / "README.md"
MACHINE = "m5"

NAMES = {
    "native": "Mac itself",
    "lighter": "lighter",
    "orbstack": "OrbStack",
    "docker-desktop": "Docker Desktop",
    "colima": "Colima",
    "podman": "Podman",
    "apple-container": "Apple container",
}
RUNTIMES = ["lighter", "orbstack", "docker-desktop", "colima", "podman", "apple-container"]
STAGES = {"": "share", "-guest": "guest", "-media": "media"}


def secs(ms, places=1):
    return f"{ms / 1000:.{places}f} s"


def millis(ms):
    return f"{ms:.0f} ms"


def mib(v):
    return f"{v:,.0f} MiB"


def micros(v):
    return f"{v:.0f} µs"


def gbit(v):
    return f"{v / 1000:.1f}"


def per_second(v):
    return f"{v:.0f} ms/s"


# (case, label, format, lower is better). The Mac itself is a column of the
# shared-folder and media tables only: its engine rows do not exist.
SHARE = [
    ("npm-install", "npm install", secs, True),
    ("pnpm-install", "pnpm install", secs, True),
    ("yarn-install", "yarn install", secs, True),
    ("ripgrep", "ripgrep over the tree", millis, True),
    ("find-walk", "find over the tree", millis, True),
    ("copy-tree", "copy the tree", secs, True),
    ("rm-rf", "rm -rf the tree", secs, True),
    ("disk-seq-write", "write 1 GiB, fsynced", millis, True),
]
GUEST = [
    ("npm-install", "npm install", secs, True),
    ("pnpm-install", "pnpm install", secs, True),
    ("yarn-install", "yarn install", secs, True),
    ("ripgrep", "ripgrep over the tree", millis, True),
    ("find-walk", "find over the tree", millis, True),
    ("copy-tree", "copy the tree", lambda v: secs(v, 2), True),
    ("rm-rf", "rm -rf the tree", lambda v: secs(v, 2), True),
    ("disk-seq-write", "write 1 GiB, fsynced", millis, True),
]
ENGINE = [
    ("container-start", "container start", millis, True),
    ("boot-first-container", "boot to first container", millis, True),
    ("memory-idle-1", "memory, one idle container", mib, True),
    ("memory-peak", "memory, peak during an install", mib, True),
    ("memory-after-60s", "memory, a minute after it", mib, True),
    ("power-cpu-ms-per-s", "idle CPU", per_second, True),
    ("net-tcp-egress", "TCP, Mac to container (Gbit/s)", gbit, False),
    ("net-tcp-egress-r", "TCP, container to Mac (Gbit/s)", gbit, False),
    ("net-tcp-port", "TCP, published port (Gbit/s)", gbit, False),
    ("net-udp", "UDP (Gbit/s)", gbit, False),
    ("net-http-latency", "HTTP GET on a published port, median", micros, True),
    ("net-dns", "DNS lookup", micros, True),
    ("watch-latency", "a host change seen in a container", millis, True),
    ("cpu-sha256", "sha256 of 1 GiB (CPU)", secs, True),
]
MEDIA = [
    ("transcode-h264", "transcode to H.264, 10 s of 1080p30", lambda v: secs(v, 2), True),
    ("transcode-hevc", "transcode to HEVC, the same", lambda v: secs(v, 2), True),
    ("cpu-zstd-8", "zstd -9, 256 MiB, 8 threads", lambda v: secs(v, 2), True),
    ("llm", "LLM on the CPU (Qwen2.5 0.5B, 8 threads)", lambda v: secs(v, 2), True),
]
# Cells that read as something other than their number.
# Apple's container runs a VM per container and none once it exits, so its
# reading a minute after the install is of no VM at all.
SPECIAL = {("apple-container", "memory-after-60s"): "–†"}
# A UDP run that moved nothing failed.
ZERO_FAILS = {"net-udp"}
# The media engine: the Mac and lighter encode on it, everyone else in software.
ENGINE_MARK = {("native", "transcode-h264"), ("native", "transcode-hevc"),
               ("lighter", "transcode-h264"), ("lighter", "transcode-hevc")}


def load(root):
    """(target, stage) -> case -> median, or None when a repetition failed."""
    runs = {}
    for path in sorted(root.glob(f"{MACHINE}-*.csv")):
        name = path.stem[len(MACHINE) + 1:]
        for suffix, stage in sorted(STAGES.items(), key=lambda s: -len(s[0])):
            if suffix and name.endswith(suffix):
                target = name[: -len(suffix)]
                break
        else:
            target, stage = name, "share"
        if target not in NAMES:
            continue
        rows = list(csv.DictReader(path.open()))
        if not rows or "case" not in rows[0]:
            continue
        cases = {}
        for row in rows:
            cases.setdefault(row["case"], []).append(row["ms"])
        runs[(target, stage)] = {
            case: None if any(not re.fullmatch(r"[0-9.]+", v) for v in values)
            else statistics.median(float(v) for v in values)
            for case, values in cases.items()
        }
    return runs


def table(runs, stage, rows, columns):
    out = ["| | " + " | ".join(NAMES[c] for c in columns) + " |", "|---|" + "---|" * len(columns)]
    for case, label, fmt, lower in rows:
        values = {}
        for c in columns:
            v = runs.get((c, stage), {}).get(case)
            if v is not None and case in ZERO_FAILS and v == 0:
                v = None
            values[c] = v
        ranked = [values[c] for c in RUNTIMES if c in columns and values[c] is not None and (c, case) not in SPECIAL]
        best = (min if lower else max)(ranked) if ranked else None
        cells = []
        for c in columns:
            if (c, case) in SPECIAL:
                cells.append(SPECIAL[(c, case)])
                continue
            v = values[c]
            if v is None:
                cells.append("failed")
                continue
            text = fmt(v) + ("ᵐ" if (c, case) in ENGINE_MARK else "")
            cells.append(f"**{text}**" if c != "native" and v == best else text)
        out.append(f"| {label} | " + " | ".join(cells) + " |")
    return "\n".join(out)


def gpu(root):
    rows = {}
    path = root / f"{MACHINE}-llm-gpu.csv"
    for line in path.read_text().splitlines() if path.exists() else []:
        parts = line.split(",")
        if len(parts) == 4 and parts[0] != "target":
            rows[(parts[0], parts[1], parts[2])] = float(parts[3])
    out = ["| | prompt | generation |", "|---|---|---|"]
    for target, backend, label in [
        ("native", "metal", "Mac itself, Metal"),
        ("lighter", "metal", "**lighter, Metal**"),
        ("lighter", "vulkan", "lighter, Vulkan"),
        ("podman", "vulkan", "Podman, Vulkan"),
    ]:
        pp, tg = rows.get((target, backend, "pp512")), rows.get((target, backend, "tg128"))
        if pp is None or tg is None:
            continue
        cells = [f"{pp:,.0f}", f"{tg:,.0f}"]
        if target == "lighter" and backend == "metal":
            cells = [f"**{c}**" for c in cells]
        out.append(f"| {label} | " + " | ".join(cells) + " |")
    return "\n".join(out)


def section(root):
    runs = load(root)
    rel = root.relative_to(HERE.parent)
    everyone = ["native"] + RUNTIMES
    return f"""## Benchmarks

Seven ways to run a container on a MacBook Pro (M5 Pro, 18 cores, 48 GB, macOS 26), every runtime at 8 vCPUs and 16 GiB, medians of three, recorded with `benchmarks/compare.sh` on lighter 0.10.0. The method, the raw results and every caveat are in [the record]({rel}/README.md).

**On a folder shared from the Mac**

{table(runs, "share", SHARE, everyone)}

**On the runtime's own disk**

{table(runs, "guest", GUEST, RUNTIMES)}

**Engine**

{table(runs, "share", ENGINE, RUNTIMES)}

† Apple container runs a VM per container, and none once it exits.

lighter keeps a container's file cache for 30 s after it stops, so the next command reads what the last one wrote from memory; that is its higher peak, and it has given the memory back a minute later.

**Media and an LLM**

{table(runs, "media", MEDIA, everyone)}

ᵐ on the Mac's media engine, with output identical to the Mac's own; every other runtime encodes in software.

**An LLM on the GPU** (llama.cpp, Qwen2.5 0.5B Q4_K_M, tokens a second)

{gpu(root)}

OrbStack, Docker Desktop, Colima and Apple container give a container no GPU.

---

"""


def main():
    args = [a for a in sys.argv[1:] if a != "--write"]
    root = Path(args[0]).resolve() if args else sorted((HERE / "results" / "machines" / MACHINE).glob("compare-*"))[-1]
    text = section(root)
    if "--write" not in sys.argv:
        print(text, end="")
        return
    readme = README.read_text()
    start = readme.index("## Benchmarks\n")
    end = readme.index("\n## ", start + 1) + 1
    README.write_text(readme[:start] + text + readme[end:])


if __name__ == "__main__":
    main()
