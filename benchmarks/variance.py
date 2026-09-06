#!/usr/bin/env python3
"""Describe repeatability from fresh runs of one build, never pooled versions.

Usage: python3 benchmarks/variance.py <run-1.csv> <run-2.csv> <run-3.csv> ...
Each CSV must have three completed repetitions per case and a matching .tree.
"""

import argparse
import csv
import math
from pathlib import Path
import statistics


def load(path):
    stamp = dict(line.split("=", 1) for line in path.with_suffix(".tree").read_text().splitlines())
    identity = {key: value for key, value in stamp.items() if key != "date"}
    if not identity.get("commit") or "dirty" in identity["commit"]:
        raise ValueError(f"{path}: missing or dirty source stamp")
    values = {}
    with path.open() as stream:
        reader = csv.DictReader(stream)
        if reader.fieldnames != ["case", "rep", "ms"]:
            raise ValueError(f"{path}: unexpected CSV header")
        for row in reader:
            rep, value = int(row["rep"]), float(row["ms"])
            case = values.setdefault(row["case"], {})
            if rep in case or not math.isfinite(value) or value <= 0:
                raise ValueError(f"{path}: duplicate repetition or invalid timing")
            case[rep] = value
    if not values or any(set(case) != {1, 2, 3} for case in values.values()):
        raise ValueError(f"{path}: each case needs repetitions 1, 2 and 3")
    return identity, {case: list(reps.values()) for case, reps in values.items()}


def cv(values):
    return statistics.stdev(values) / statistics.mean(values) * 100


def report(paths):
    if len(paths) < 3 or len({path.resolve() for path in paths}) != len(paths):
        raise ValueError("provide at least three distinct fresh-run CSVs")
    loaded = [load(path) for path in paths]
    identity, first = loaded[0]
    if any(stamp != identity or set(values) != set(first) for stamp, values in loaded):
        raise ValueError("runs differ in source, artifacts, host or case set; do not pool them")
    lines = [
        f"{len(paths)} fresh runs of `{identity['commit']}` on `{identity.get('host', 'unknown')}`. "
        "All timings are milliseconds. Each run contributes its median of three repetitions.",
        "",
        "| Case | Run medians, in input order | Median | Between-run SD | Between-run CV | Observed range | Within-run CV, median |",
        "|---|---|---:|---:|---:|---|---:|",
    ]
    for case in first:
        samples = [values[case] for _, values in loaded]
        medians = [statistics.median(values) for values in samples]
        lines.append(
            f"| {case} | {', '.join(f'{value:g}' for value in medians)} "
            f"| {statistics.median(medians):g} | {statistics.stdev(medians):.1f} "
            f"| {cv(medians):.1f}% | {min(medians):g}–{max(medians):g} "
            f"| {statistics.median(cv(values) for values in samples):.1f}% |"
        )
    lines.extend([
        "",
        "SD is sample standard deviation; CV is SD divided by the mean. "
        "Between-run CV describes the fresh-run medians. Within-run CV describes "
        "the three ordered repetitions and can include cache/order effects.",
        "",
        "The observed range is descriptive, not a confidence interval or an automatic "
        "regression threshold. Same-build variation does not prove that a similarly "
        "sized cross-version change is noise. Use repeated, alternating version runs "
        "to check an apparent change.",
    ])
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("csvs", nargs="+", type=Path)
    args = parser.parse_args()
    try:
        print(report(args.csvs), end="")
    except (OSError, ValueError, KeyError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
