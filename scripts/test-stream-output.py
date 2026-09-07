#!/usr/bin/env python3
"""Stress Docker stream framing and published response drainage on a test VM."""

import argparse
import concurrent.futures as cf
import hashlib
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess as sp
import time
import uuid


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--image", required=True)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--reps", type=int, default=100)
    ap.add_argument("--workers", type=int, default=8)
    a = ap.parse_args()
    if not os.environ.get("DOCKER_HOST") or os.environ.get("DOCKER_CONTEXT"):
        ap.error("set DOCKER_HOST and unset DOCKER_CONTEXT")
    home = Path(os.environ.get("LIGHTER_HOME", "")).resolve()
    if (
        not os.environ.get("LIGHTER_HOME")
        or home == (Path.home() / ".lighter").resolve()
    ):
        ap.error("requires an isolated LIGHTER_HOME")
    if os.environ["DOCKER_HOST"] != f"unix://{home}/docker.sock":
        ap.error("DOCKER_HOST must select LIGHTER_HOME")
    if a.output.exists() or a.reps < 1 or a.workers < 1:
        ap.error("use a new output and positive counts")
    name = "lighter-stream-" + uuid.uuid4().hex[:10]
    payload = bytes(range(256)) * 4096
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    server = """import http.server
payload=bytes(range(256))*4096
class H(http.server.BaseHTTPRequestHandler):
 protocol_version="HTTP/1.1"
 def do_GET(self):
  body=payload*16 if self.path=="/large" else payload
  self.send_response(200);self.send_header("Content-Length",str(len(body)))
  if self.headers.get("X-Close"):self.send_header("Connection","close");self.close_connection=True
  self.end_headers();self.wfile.write(body)
 def log_message(self,*args):pass
http.server.ThreadingHTTPServer(("0.0.0.0",8080),H).serve_forever()
"""
    failures = 0

    def docker_task(i):
        mode = i % 4
        args = ["docker", "exec"]
        data = None
        if mode < 2:
            size = 17 if mode == 0 else len(payload)
            expected = payload[:size]
            error = expected[::-1]
            code = f"import sys; p=bytes(range(256))*4096;sys.stdout.buffer.write(p[:{size}]);sys.stderr.buffer.write(p[:{size}][::-1])"
        else:
            args.append("-i")
            data = payload[:23] if mode == 2 else payload * 2
            expected = data
            error = b""
            code = "import sys;sys.stdout.buffer.write(sys.stdin.buffer.read())"
        p = sp.run(
            args + [name, "python3", "-c", code],
            input=data,
            capture_output=True,
            timeout=60,
        )
        return dict(
            kind="docker",
            iteration=i,
            mode=mode,
            passed=p.returncode == 0 and p.stdout == expected and p.stderr == error,
            rc=p.returncode,
            expected_stdout=len(expected),
            stdout=len(p.stdout),
            expected_stderr=len(error),
            stderr=len(p.stderr),
            stdout_sha256=hashlib.sha256(p.stdout).hexdigest(),
            stderr_sha256=hashlib.sha256(p.stderr).hexdigest(),
        )

    def http_task(i):
        c = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
        try:
            for repeat in range(3 if i % 3 == 0 else 1):
                c.request("GET", "/", headers={"X-Close": "1"} if i % 3 == 1 else {})
                if i % 3 == 2:
                    c.sock.shutdown(socket.SHUT_WR)
                r = c.getresponse()
                body = r.read()
                if r.status != 200 or body != payload:
                    return dict(
                        kind="http",
                        iteration=i,
                        passed=False,
                        status=r.status,
                        bytes=len(body),
                    )
            return dict(kind="http", iteration=i, passed=True)
        finally:
            c.close()

    def backpressure_task(i):
        c = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
        try:
            c.request("GET", "/large", headers={"X-Close": "1"})
            r = c.getresponse()
            time.sleep(0.1)
            if i % 2:
                # Abort with queued response bytes, then verify a fresh stream.
                c.close()
                row = http_task(i)
                row["kind"] = "aborted-peer-recovery"
                return row
            body = bytearray()
            while chunk := r.read(16384):
                body.extend(chunk)
                time.sleep(0.001)
            return dict(
                kind="slow-reader",
                iteration=i,
                passed=r.status == 200 and body == payload * 16,
                bytes=len(body),
            )
        finally:
            c.close()

    pressure_reps = max(10, a.reps // 5)
    try:
        sp.run(
            [
                "docker",
                "run",
                "-d",
                "--name",
                name,
                "-p",
                f"127.0.0.1:{port}:8080",
                a.image,
                "python3",
                "-u",
                "-c",
                server,
            ],
            check=True,
            capture_output=True,
            timeout=120,
        )
        # Readiness only; workload failures below are never retried.
        for _ in range(60):
            try:
                c = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
                c.request("GET", "/")
                r = c.getresponse()
                assert r.read() == payload
                c.close()
                break
            except Exception:
                time.sleep(0.2)
        else:
            raise RuntimeError("HTTP server did not become ready")
        with a.output.open("x") as out, cf.ThreadPoolExecutor(
            max_workers=a.workers
        ) as pool:
            futures = {
                pool.submit(fn, i): (fn.__name__, i)
                for fn in [docker_task, http_task]
                for i in range(a.reps * 4)
            }
            futures.update({
                pool.submit(backpressure_task, i): ("backpressure_task", i)
                for i in range(pressure_reps)
            })
            for f in cf.as_completed(futures):
                try:
                    row = f.result()
                except Exception as e:
                    kind, iteration = futures[f]
                    row = dict(kind=kind, iteration=iteration, passed=False,
                               error=repr(e))
                out.write(json.dumps(row) + "\n")
                out.flush()
                if not row["passed"]:
                    failures += 1
                    print(json.dumps(row), flush=True)
    finally:
        p = sp.run(["docker", "rm", "-f", name], capture_output=True, timeout=60)
        if p.returncode:
            failures += 1
            print("container cleanup failed", flush=True)
    print(
        f"{a.reps*8 + pressure_reps} concurrent Docker/HTTP checks; {failures} failures"
    )
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
