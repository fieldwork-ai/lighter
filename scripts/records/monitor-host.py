#!/usr/bin/env python3
"""Observe fseventsd and Lighter task footprints without resetting either."""

import argparse
import datetime
import json
from pathlib import Path
import re
import signal
import subprocess as sp
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "benchmarks"))
from guard import processes, vm_kind


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("directory", type=Path)
    a = ap.parse_args()
    a.directory.mkdir(parents=True, exist_ok=True)
    probe = ROOT / "target/benchmarks/task-footprint"
    if not probe.is_file():
        ap.error("build target/benchmarks/task-footprint before recording")
    stop = False

    def stopping(*_):
        nonlocal stop
        stop = True

    signal.signal(signal.SIGTERM, stopping)
    signal.signal(signal.SIGINT, stopping)

    def mib(value):
        match = re.match(r"([0-9.]+)([BKMG])", value)
        return (
            float(match[1])
            * dict(B=1, K=1024, M=1024**2, G=1024**3)[match[2]]
            / 1024**2
        )

    with (a.directory / "daemon.jsonl").open("x") as rows, (
        a.directory / "daemon.top"
    ).open("x") as raw:
        while not stop:
            begin = time.monotonic()
            entry = dict(utc=datetime.datetime.now(datetime.timezone.utc).isoformat())
            phase = a.directory / "phase"
            entry["phase"] = (
                phase.read_text().strip() if phase.exists() else "unspecified"
            )
            try:
                pid = int(
                    sp.check_output(
                        ["pgrep", "-x", "fseventsd"], text=True, timeout=5
                    ).strip()
                )
                top = sp.check_output(
                    [
                        "top",
                        "-l",
                        "2",
                        "-s",
                        "1",
                        "-pid",
                        str(pid),
                        "-stats",
                        "pid,command,cpu,mem,cmprs",
                    ],
                    text=True,
                    timeout=5,
                )
                raw.write(top)
                raw.flush()
                row = [
                    line.split()
                    for line in top.splitlines()
                    if re.match(r"^" + str(pid) + r"\s", line)
                ][-1]
                entry.update(
                    pid=pid,
                    cpu_percent=float(row[2]),
                    footprint_mib=mib(row[3]),
                    compressed_mib=mib(row[4]),
                )
                host = processes()
                machines = []
                for process in host.values():
                    kind = vm_kind(process)
                    if kind:
                        record = dict(process, kind=kind)
                        if kind == "lighter":
                            measured = sp.run(
                                [str(probe), str(process["pid"])],
                                capture_output=True,
                                text=True,
                                timeout=3,
                            )
                            record["footprint_bytes"] = (
                                int(measured.stdout)
                                if measured.returncode == 0
                                else None
                            )
                        machines.append(record)
                entry["vms"] = machines
                entry["top_processes"] = sorted(
                    host.values(), key=lambda process: process["cpu"], reverse=True
                )[:15]
            except Exception as error:
                entry["error"] = str(error)
            rows.write(json.dumps(entry) + "\n")
            rows.flush()
            while not stop and time.monotonic() - begin < 5:
                time.sleep(0.2)


if __name__ == "__main__":
    main()
