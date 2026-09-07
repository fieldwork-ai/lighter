#!/usr/bin/env python3
"""Run a benchmark with continuous competing-VM checks and a quiet preflight.

Only VMs descended from the command (for a lighter target), or explicitly
listed by PID, are allowed. An unexpected VM invalidates and stops our command;
it never kills the competing VM. All observations are retained as JSONL.
"""

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess as sp
import time
import tempfile


def processes():
    rows = sp.check_output(["ps", "-axo", "pid=,ppid=,pcpu=,comm="], text=True)
    result = {}
    for line in rows.splitlines():
        pid, parent, cpu, command = line.strip().split(None, 3)
        result[int(pid)] = dict(
            pid=int(pid), parent=int(parent), cpu=float(cpu), command=command
        )
    return result


def vm_kind(process):
    command = process["command"].lower()
    base = Path(command).name
    if base == "lighter-bench" or "/lighter.app/contents/macos/lighter" in command:
        return "lighter"
    if base == "lighter":
        args = sp.run(
            ["ps", "-p", str(process["pid"]), "-o", "args="],
            capture_output=True,
            text=True,
        ).stdout
        if " run" in args:
            return "lighter"
    if base == "limactl":
        args = sp.run(
            ["ps", "-p", str(process["pid"]), "-o", "args="],
            capture_output=True,
            text=True,
        ).stdout
        if "hostagent" in args:
            return "lima"
    if any(
        token in command
        for token in ["qemu-system", "vfkit", "virtualization.virtualmachine"]
    ):
        return "hypervisor"
    if "orbstack" in command and base in ["orbstack", "orbstack helper", "xbin"]:
        return "orbstack"
    if base in ["com.docker.backend", "com.docker.virtualization", "docker vmm"]:
        return "docker-desktop"
    return None


def descendant(pid, roots, table):
    seen = set()
    while pid in table and pid not in seen:
        if pid in roots:
            return True
        seen.add(pid)
        pid = table[pid]["parent"]
    return False


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--log", type=Path, required=True)
    ap.add_argument("--timeout", type=int, default=3600)
    ap.add_argument("--quiet", action="store_true")
    ap.add_argument("--allow-pid", type=int, action="append", default=[])
    ap.add_argument(
        "--target",
        choices=["lighter", "native", "colima", "orbstack", "docker-desktop"],
        required=True,
    )
    ap.add_argument("command", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    command = a.command[1:] if a.command[:1] == ["--"] else a.command
    if not command or a.log.exists():
        ap.error("provide a command and a new log path")
    count = int(sp.check_output(["sysctl", "-n", "hw.ncpu"], text=True))
    child = None
    owner_handle = tempfile.NamedTemporaryFile(
        prefix="benchmark-owners-", dir=a.log.parent, delete=False
    )
    owner_path = Path(owner_handle.name)
    owner_handle.close()
    allowed = set(a.allow_pid)
    # Keep a PID once its ancestry establishes ownership: some VM helpers daemonize.
    owned = set()
    failure = None
    with a.log.open("x") as log:

        def sample(phase):
            table = processes()
            allowed.intersection_update(table)
            paths = {
                str(Path(json.loads(line)).resolve())
                for line in owner_path.read_text().splitlines()
            }
            machines = []
            bad = []
            for pid, process in table.items():
                kind = vm_kind(process)
                if not kind:
                    continue
                legitimate = descendant(pid, allowed | owned, table)
                if (
                    kind == a.target == "lighter"
                    and str(Path(process["command"]).resolve()) in paths
                ):
                    legitimate = True
                if (
                    child
                    and kind == a.target == "lighter"
                    and descendant(pid, {child.pid}, table)
                ):
                    legitimate = True
                if legitimate:
                    owned.add(pid)
                else:
                    bad.append(pid)
                machines.append(dict(process, kind=kind, allowed=legitimate))
            # Remove dead owners so a recycled PID cannot grant future permission.
            owned.intersection_update(table)
            background = sum(p["cpu"] for p in table.values()) / count
            log.write(
                json.dumps(
                    dict(
                        time=time.time(),
                        phase=phase,
                        machine_cpu_percent=background,
                        vms=machines,
                        unexpected=bad,
                    )
                )
                + "\n"
            )
            log.flush()
            if bad:
                raise RuntimeError("competing VM PIDs: " + ", ".join(map(str, bad)))
            return background

        try:
            sample("preflight")
            if a.quiet:
                streak = 0
                for _ in range(90):
                    streak = streak + 1 if sample("quiet") <= 5 else 0
                    if streak >= 6:
                        break
                    time.sleep(10)
                else:
                    raise RuntimeError(
                        "host did not meet six consecutive <=5% CPU samples in 15 minutes"
                    )
            child = sp.Popen(
                command,
                start_new_session=True,
                env=dict(
                    os.environ, LIGHTER_BENCH_OWNER_FILE=str(owner_path.resolve())
                ),
            )
            deadline = time.monotonic() + a.timeout
            while child.poll() is None:
                sample("running")
                if time.monotonic() >= deadline:
                    raise RuntimeError("benchmark deadline exceeded")
                time.sleep(1)
            sample("finished")
            code = child.returncode
        except BaseException as error:
            failure = str(error) or type(error).__name__
            print("INVALID:", failure, flush=True)
            code = 1
        finally:
            if child and child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(15)
                except sp.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
            log.write(
                json.dumps(
                    dict(
                        result="invalid" if failure else "complete",
                        error=failure,
                        exit_code=code,
                    )
                )
                + "\n"
            )
    owner_path.unlink()
    return code


if __name__ == "__main__":
    raise SystemExit(main())
