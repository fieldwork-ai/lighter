#!/usr/bin/env python3
"""Repeat published-container lifecycles; retain the VM and logs on failure.

Run on an otherwise idle host, outside benchmark measurements. Each round
starts two containers, checks IPv4, IPv6, LAN and loopback-only publishing,
then removes both together. All VM storage is private to this test.
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
HTTP = 'while true; do printf "HTTP/1.0 200 OK\\r\\nContent-Length: 2\\r\\n\\r\\nok" | nc -l -p 80; done'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rounds", type=int, default=100)
    parser.add_argument("--lan-ip", help="defaults to the address of en0 or en1")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error("--rounds must be positive")
    lan = args.lan_ip
    if not lan:
        for interface in ("en0", "en1"):
            result = subprocess.run(
                ["ipconfig", "getifaddr", interface], capture_output=True, text=True
            )
            if result.returncode == 0 and result.stdout.strip():
                lan = result.stdout.strip()
                break
    if not lan:
        parser.error("no LAN address found; supply --lan-ip")
    for port in (18097, 18098):
        with socket.socket() as probe:
            probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            probe.bind(("0.0.0.0", port))

    out = args.output or ROOT / ".logs" / ("publish-churn-" + time.strftime("%Y%m%d-%H%M%S"))
    out.mkdir(parents=True, exist_ok=False)
    home = Path(tempfile.mkdtemp(prefix="lighter-publish-churn-"))
    env = {**os.environ, "LIGHTER_HOME": str(home)}
    lighter = str(ROOT / "target/release/lighter")
    docker = ["docker", "-H", "unix://" + str(home / "docker.sock")]
    state = {"home": str(home), "rounds": args.rounds, "lan_ip": lan}

    def save():
        (out / "state.json").write_text(json.dumps(state, indent=2) + "\n")
        if (home / "machine.log").exists():
            shutil.copyfile(home / "machine.log", out / "machine.log")

    def call(command, timeout=15):
        result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=timeout)
        if result.returncode:
            raise RuntimeError(f"{command}: {result.stderr[-2000:]}")
        return result.stdout

    save()
    print(f"Evidence: {out}", flush=True)
    try:
        print(call([lighter, "start", "--timeout", "60"], 75), flush=True)
        state["vm"] = int((home / "lighter.pid").read_text())
        call(docker + ["pull", "alpine:3.21"], 90)
        for index in range(args.rounds):
            state["pair"] = index + 1
            for name, port in (("churn-a", "18098:80"), ("churn-b", "127.0.0.1:18097:80")):
                call(docker + ["run", "-d", "--rm", "--name", name, "-p", port,
                               "alpine:3.21", "sh", "-c", HTTP])
            time.sleep(0.25)
            for endpoint in ("127.0.0.1:18098", "[::1]:18098", lan + ":18098", "127.0.0.1:18097"):
                state["endpoint"] = endpoint
                for _ in range(15):
                    result = subprocess.run(
                        ["curl", "--noproxy", "*", "-fsS", "--max-time", "1", "http://" + endpoint + "/"],
                        capture_output=True, text=True, timeout=3,
                    )
                    if result.returncode == 0 and result.stdout == "ok":
                        break
                    time.sleep(0.1)
                else:
                    raise RuntimeError("HTTP listener did not answer: " + endpoint)
            if index % 5 == 0:
                time.sleep(2)
            state["endpoint"] = "remove both containers"
            call(docker + ["rm", "-f", "churn-a", "churn-b"])
            print(f"PASS pair {index + 1}", flush=True)
            if index % 10 == 0:
                save()
        call([lighter, "stop"], 30)
        state["result"] = "PASS"
        save()
        shutil.rmtree(home)
        print("CHURN COMPLETE", flush=True)
    except BaseException as error:
        state["result"] = repr(error)
        if (home / "lighter.pid").exists():
            state["vm"] = int((home / "lighter.pid").read_text())
            try:
                os.kill(state["vm"], signal.SIGINFO)
                time.sleep(1)
            except ProcessLookupError:
                pass
        save()
        print(f"Failed; private VM retained for diagnosis: {home}", flush=True)
        raise


if __name__ == "__main__":
    main()
