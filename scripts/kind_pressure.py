"""Mixed Docker/build/Kubernetes memory qualification used by test-kind.py."""

import json
import os
from pathlib import Path
import socket
import subprocess as sp
import threading
import time


def qualify(*, home, out, cmd, k, image, base_image, node, namespace, service_check):
    root = Path(__file__).resolve().parents[1]
    probe = root / "target/benchmarks/task-footprint"
    probe.parent.mkdir(parents=True, exist_ok=True)
    sp.run(
        [
            "cc",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            str(root / "benchmarks/task-footprint.c"),
            "-o",
            str(probe),
        ],
        check=True,
    )
    configured = json.loads((home / "config.json").read_text())["memory_mib"]
    if configured not in [8192, 12288, 16384]:
        raise RuntimeError("mixed pressure qualification requires 8, 12 or 16 GiB")
    pid = int((home / "lighter.pid").read_text())
    samples = []
    stop = threading.Event()
    phase = "baseline"
    holder = "lighter-memory-" + out.name.lower().replace(".", "-")
    pod = "memory-holder"
    allocations = dict(
        pod=configured * 25 // 100,
        docker=configured * 30 // 100,
        build=configured * 15 // 100,
    )
    # Random pages cannot disappear into zero-page/compression accounting.
    code = """import mmap,os,sys,time
size=int(sys.argv[1])*1024*1024
m=mmap.mmap(-1,size)
block=os.urandom(1024*1024)
for offset in range(0,size,len(block)):m[offset:offset+len(block)]=block
print("allocated",size,flush=True)
time.sleep(60)
"""

    def monitor():
        with (out / "memory-samples.jsonl").open("x") as log:
            while not stop.is_set():
                row = dict(time=time.time(), phase=phase)
                try:
                    row["footprint_mib"] = (
                        int(sp.check_output([str(probe), str(pid)], timeout=3))
                        / 1024**2
                    )
                    with socket.socket(
                        socket.AF_UNIX, socket.SOCK_STREAM
                    ) as connection:
                        connection.settimeout(3)
                        connection.connect(str(home / "control.sock"))
                        connection.sendall(
                            b"sh cat /proc/meminfo; if test -r /proc/pressure/memory; then cat /proc/pressure/memory; fi\n"
                        )
                        data = b""
                        while not data.endswith(b"--end--\n"):
                            block = connection.recv(8192)
                            if not block:
                                raise RuntimeError(
                                    "control stream ended without terminator"
                                )
                            data += block
                    row["guest"] = data.decode()
                    row["guest_total_mib"] = (
                        int(
                            next(
                                line
                                for line in row["guest"].splitlines()
                                if line.startswith("MemTotal:")
                            ).split()[1]
                        )
                        / 1024
                    )
                except Exception as error:
                    row["error"] = str(error)
                samples.append(row)
                log.write(json.dumps(row) + "\n")
                log.flush()
                stop.wait(0.5)

    thread = threading.Thread(target=monitor)
    thread.start()
    build = None
    build_log = None
    verdict = {}
    try:
        time.sleep(5)
        baseline = max(
            row["footprint_mib"] for row in samples if "footprint_mib" in row
        )
        (out / "memory-pod.json").write_text(
            json.dumps(
                dict(
                    apiVersion="v1",
                    kind="Pod",
                    metadata=dict(name=pod, namespace=namespace),
                    spec=dict(
                        restartPolicy="Never",
                        nodeName=node,
                        tolerations=[dict(operator="Exists")],
                        containers=[
                            dict(
                                name="holder",
                                image=image,
                                imagePullPolicy="Never",
                                command=[
                                    "python3",
                                    "-u",
                                    "-c",
                                    code,
                                    str(allocations["pod"]),
                                ],
                                resources=dict(
                                    requests=dict(memory=f"{allocations['pod']}Mi"),
                                    limits=dict(memory=f"{allocations['pod']+256}Mi"),
                                ),
                            )
                        ],
                    ),
                )
            )
        )
        phase = "pressure"
        k("apply", "-f", str(out / "memory-pod.json"))
        cmd(
            [
                "docker",
                "run",
                "-d",
                "--name",
                holder,
                "--memory",
                f"{allocations['docker']+256}m",
                base_image,
                "python3",
                "-u",
                "-c",
                code,
                str(allocations["docker"]),
            ],
            120,
        )
        build_dir = out / "memory-build"
        build_dir.mkdir()
        (build_dir / "allocate.py").write_text(code)
        (build_dir / "Dockerfile").write_text(
            f'FROM {base_image}\nCOPY allocate.py /allocate.py\nRUN python3 -u /allocate.py {allocations["build"]}\n'
        )
        build_log = (out / "memory-build.log").open("w")
        build = sp.Popen(
            [
                "docker",
                "build",
                "--no-cache",
                "--progress=plain",
                "-t",
                holder + ":built",
                str(build_dir),
            ],
            stdout=build_log,
            stderr=sp.STDOUT,
        )
        k(
            "-n",
            namespace,
            "wait",
            "--for=condition=Ready",
            "pod/" + pod,
            "--timeout=90s",
            timeout=100,
        )
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if (
                "allocated " in k("-n", namespace, "logs", pod)
                and "allocated " in cmd(["docker", "logs", holder]).stdout
                and "allocated " in (out / "memory-build.log").read_text()
            ):
                break
            time.sleep(1)
        else:
            raise RuntimeError("not all three simultaneous allocations completed")
        for _ in range(10):
            service_check()
            time.sleep(1)
        if build.wait(150):
            raise RuntimeError("memory-consuming Docker build failed")
        if cmd(["docker", "wait", holder], 120).stdout.strip() != "0":
            raise RuntimeError("Docker memory holder failed")
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            state = json.loads(k("-n", namespace, "get", "pod", pod, "-o", "json"))[
                "status"
            ]["phase"]
            if state == "Succeeded":
                break
            if state == "Failed":
                raise RuntimeError("Kubernetes memory holder failed")
            time.sleep(1)
        else:
            raise RuntimeError("Kubernetes memory holder did not finish")
        service_check()
        phase = "recovery"
        k("-n", namespace, "delete", "pod", pod, "--wait=true", "--timeout=30s")
        cmd(["docker", "rm", holder])
        cmd(["docker", "image", "rm", holder + ":built"])
        peak = max(row["footprint_mib"] for row in samples if "footprint_mib" in row)
        growth = peak - baseline
        if growth < sum(allocations.values()) * 0.65:
            raise RuntimeError("footprint did not observe the simultaneous allocations")
        deadline = time.monotonic() + 180
        recovered = False
        while time.monotonic() < deadline:
            recovery = [
                row["footprint_mib"]
                for row in samples
                if row["phase"] == "recovery" and "footprint_mib" in row
            ]
            if recovery and peak - min(recovery) >= growth * 0.70:
                recovered = True
                break
            time.sleep(1)
        if not recovered:
            raise RuntimeError(
                "less than 70% of allocation footprint returned within 180s"
            )
        service_check()
        if any("error" in row for row in samples):
            raise RuntimeError("memory sampler has errors")
        if any(row["guest_total_mib"] > configured for row in samples):
            raise RuntimeError("guest memory exceeded configuration")
        # Guest RAM is the configured ceiling; the VMM also needs host buffers,
        # page tables and stacks. Report the excess explicitly, never call it RAM.
        if peak > configured + 1024:
            raise RuntimeError(
                "host footprint exceeded guest ceiling plus 1 GiB overhead budget"
            )
        verdict = dict(
            passed=True,
            configured_mib=configured,
            allocations_mib=allocations,
            baseline_mib=baseline,
            peak_mib=peak,
            host_excess_mib=max(0, peak - configured),
            best_recovery_mib=min(recovery),
            returned_fraction=(peak - min(recovery)) / growth,
            samples=len(samples),
            sampling_interval_s=0.5,
            host_overhead_budget_mib=1024,
        )
    finally:
        stop.set()
        thread.join(10)
        if build and build.poll() is None:
            build.terminate()
            try:
                build.wait(10)
            except sp.TimeoutExpired:
                build.kill()
                build.wait()
        if build_log:
            build_log.close()
        # Only this test's explicitly named resources may be removed.
        sp.run(["docker", "rm", "-f", holder], capture_output=True, timeout=30)
        (out / "memory-results.json").write_text(
            json.dumps(verdict or dict(passed=False), indent=2) + "\n"
        )
