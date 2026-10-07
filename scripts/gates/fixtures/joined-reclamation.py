#!/usr/bin/env python3
r"""Run the joined TCP/vsock reclamation regression against an isolated VM.

Build guest/out/lighter-agent with guest/agent/build.sh first. Start a separate
Lighter VM with its own LIGHTER_HOME, then pass that VM's Docker socket explicitly:

  python3 scripts/gates/fixtures/joined-reclamation.py \
      --docker-host unix:///absolute/path/to/isolated-vm/docker.sock \
      --host-ip <Mac-IP-reachable-from-the-guest>

The agent is mounted into a temporary privileged container, not installed in the
VM. --agent selects another instrumented binary for an A/B comparison. A stock
binary used for comparison needs the same --joined-reclamation-test checker,
with the original joined-stream cleanup condition retained. --image selects an
image already present in the isolated VM; Alpine is sufficient for this static
binary. The fixture needs no internet access.

The host echoes P, ensuring that the guest actually joined TCP and vsock, then
resets its TCP socket after X. The checker verifies reclamation after each of 32
connections, including zero remaining peer-map entries and a stable descriptor
count. The original condition fails with TCP HUP and vsock RDHUP without HUP;
the fixed condition succeeds. A TCP HUP while the host can still receive must
remain open, which needs separate half-close/payload integrity checks.
"""

import argparse
import ipaddress
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import threading


def fixture(listener, stop, errors):
    while not stop.is_set():
        try:
            client, _ = listener.accept()
        except socket.timeout:
            continue
        except OSError:
            return
        try:
            with client:
                client.settimeout(10)
                if client.recv(1) != b"P":
                    raise ValueError("fixture expected P")
                client.sendall(b"P")
                if client.recv(1) != b"X":
                    raise ValueError("fixture expected X")
                # On macOS SO_LINGER uses two native ints; closing now sends RST.
                client.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER,
                                  struct.pack("ii", 1, 0))
        except (OSError, ValueError) as error:
            errors.append(str(error))
            print(f"fixture: {error}", file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--docker-host", required=True, help="Docker endpoint of a separate test VM")
    parser.add_argument("--host-ip", required=True, type=ipaddress.ip_address,
                        help="Mac address reachable from the guest")
    parser.add_argument("--agent", type=Path,
                        default=Path(__file__).resolve().parents[3] / "guest/out/lighter-agent")
    parser.add_argument("--image", default="alpine:3.21", help="existing image in the isolated VM")
    parser.add_argument("--port", type=int, default=0, help="host fixture port; default allocates one")
    parser.add_argument("--timeout", type=int, default=90, help="overall checker deadline in seconds")
    args = parser.parse_args()
    agent = args.agent.resolve(strict=True)
    if args.timeout <= 0 or not 0 <= args.port <= 65535:
        parser.error("timeout must be positive and port must be between 0 and 65535")
    family = socket.AF_INET6 if args.host_ip.version == 6 else socket.AF_INET
    name = f"joined-reclamation-{os.getpid()}-{os.urandom(4).hex()}"
    docker = ["docker", "--host", args.docker_host]
    stop = threading.Event()
    errors = []
    with socket.socket(family, socket.SOCK_STREAM) as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(("::" if family == socket.AF_INET6 else "0.0.0.0", args.port))
        listener.listen(32)
        listener.settimeout(0.25)
        port = listener.getsockname()[1]
        host = f"[{args.host_ip}]" if family == socket.AF_INET6 else str(args.host_ip)
        target = f"{host}:{port}"
        print(f"fixture listening on {target}", flush=True)
        thread = threading.Thread(target=fixture, args=(listener, stop, errors), daemon=True)
        thread.start()
        command = docker + ["run", "--rm", "--name", name, "--pull", "never",
                            "--privileged", "--network", "host", "--mount",
                            f"type=bind,source={agent},target=/mnt/lighter-agent,readonly",
                            "--entrypoint", "/mnt/lighter-agent", args.image,
                            "--joined-reclamation-test", target]
        try:
            result = subprocess.run(command, timeout=args.timeout)
            return result.returncode if result.returncode != 0 else int(bool(errors))
        except subprocess.TimeoutExpired:
            print(f"checker exceeded {args.timeout}s", file=sys.stderr)
            return 1
        finally:
            stop.set()
            # Only the container created above is eligible for cleanup.
            subprocess.run(docker + ["rm", "--force", name], timeout=15,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    sys.exit(main())
