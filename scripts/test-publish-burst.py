#!/usr/bin/env python3
"""Check HTTP immediately after connection bursts on an isolated Docker VM."""
import argparse
import http.client
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import time
import uuid

RESOURCES = r"""
for p in /proc/[0-9]*; do
    comm=$(cat "$p/comm" 2>/dev/null)
    case "$comm" in
    lighter-agent|dockerd|containerd|docker-proxy)
        limits=$(awk '/Max open files/ {print $4 " " $5}' "$p/limits")
        count=$(ls -1 "$p/fd" 2>/dev/null | wc -l)
        threads=$(awk '/^Threads:/ {print $2}' "$p/status")
        cmd=$(tr '\0' ' ' < "$p/cmdline")
        printf '%s|%s|%s|%s|%s|%s\n' "$p" "$comm" "$limits" "$count" "$threads" "$cmd"
        ;;
    esac
done
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--docker-host", required=True)
    parser.add_argument("--port", type=int, default=18099)
    parser.add_argument("--rounds", type=int, default=2)
    args = parser.parse_args()
    if args.rounds < 1 or not 1024 <= args.port < 65535:
        parser.error("positive rounds and two adjacent unprivileged ports required")
    for port in (args.port, args.port + 1):
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", port))
    token = "lighter-burst-" + uuid.uuid4().hex
    out = Path(".logs") / token
    out.mkdir(parents=True)
    docker = ["docker", "-H", args.docker_host]
    inspector, server = token + "-inspect", token + "-http"
    created = []
    observations = []

    def call(command, timeout=30):
        result = subprocess.run(command, capture_output=True, text=True, timeout=timeout)
        if result.returncode:
            raise RuntimeError(f"{command}: {result.stderr[-4000:]}")
        return result.stdout

    def resources():
        rows = []
        for line in call(docker + ["exec", inspector, "sh", "-c", RESOURCES]).splitlines():
            pid, name, limits, count, threads, command = line.split("|", 5)
            soft, hard = map(int, limits.split())
            rows.append(dict(pid=pid, name=name, soft=soft, hard=hard,
                             descriptors=int(count), threads=int(threads), command=command))
        return rows

    def inbound(rows):
        return next(r for r in rows if r["name"] == "lighter-agent" and "--inbound" in r["command"])

    def check_http(port, count):
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
        try:
            for request in range(count):
                connection.request("GET", "/")
                response = connection.getresponse()
                body = response.read()
                if response.status != 200 or body != token.encode():
                    raise AssertionError(f"request {request}: status={response.status} body={body!r}")
        finally:
            connection.close()

    try:
        call(docker + ["pull", "node:22-alpine"], 120)
        call(docker + ["run", "-d", "--rm", "--name", inspector, "--pid=host",
                      "--privileged", "alpine:3.21", "sleep", "600"], 120)
        created.append(inspector)
        for mode, port in (("all-interface", args.port), ("loopback", args.port + 1)):
            publish = f"{port}:8080" if mode == "all-interface" else f"127.0.0.1:{port}:8080"
            call(docker + ["run", "-d", "--rm", "--name", server, "-p", publish,
                          "-e", f"RESPONSE={token}", "node:22-alpine", "node", "-e",
                          'require("http").createServer((q,r)=>r.end(process.env.RESPONSE)).listen(8080)'])
            created.append(server)
            for attempt in range(50):
                try:
                    check_http(port, 1)
                    break
                except (OSError, http.client.HTTPException):
                    time.sleep(0.1)
            else:
                raise AssertionError(f"{mode}: HTTP server did not become ready")
            time.sleep(0.1)
            before = resources()
            (out / f"{mode}-before.json").write_text(json.dumps(before, indent=2))
            assert before and all(r["hard"] >= 65536 and r["soft"] >= 65535 for r in before), before
            baseline = inbound(before)
            for round_number in range(1, args.rounds + 1):
                start = time.monotonic()
                for _ in range(3000):
                    with socket.create_connection(("127.0.0.1", port), timeout=5):
                        pass
                # No settling pause or retry: a real request immediately after
                # the burst must work, including all of its keep-alive requests.
                check_http(port, 2000)
                observations.append(dict(mode=mode, round=round_number,
                                         connections=3000, requests=2000,
                                         seconds=time.monotonic() - start))
                print(observations[-1], flush=True)
            deadline = time.monotonic() + 10
            while True:
                after = resources()
                current = inbound(after)
                if (current["descriptors"] <= baseline["descriptors"] + 16
                        and current["threads"] <= baseline["threads"] + 4):
                    break
                if time.monotonic() > deadline:
                    raise AssertionError(f"inbound resources did not drain: {baseline} -> {current}")
                time.sleep(0.1)
            (out / f"{mode}-after.json").write_text(json.dumps(after, indent=2))
            call(docker + ["rm", "-f", server])
            created.remove(server)
        print("published connection bursts passed; inbound resources drained", flush=True)
    finally:
        (out / "observations.json").write_text(json.dumps(observations, indent=2))
        home = os.environ.get("LIGHTER_HOME")
        if home and (Path(home) / "machine.log").exists():
            shutil.copyfile(Path(home) / "machine.log", out / "machine.log")
        for name in reversed(created):
            subprocess.run(docker + ["logs", name], stdout=(out / f"{name}.log").open("w"),
                           stderr=subprocess.STDOUT, timeout=15, check=False)
            subprocess.run(docker + ["rm", "-f", name], capture_output=True, timeout=30, check=False)


if __name__ == "__main__":
    main()
