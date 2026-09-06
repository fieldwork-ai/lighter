#!/usr/bin/env python3
"""Check external task-ledger readings against a child's known allocation."""
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix="lighter-footprint-test-") as directory:
    binary = str(Path(directory) / "task-footprint")
    subprocess.run(
        ["cc", "-O2", "-Wall", "-Wextra", "-Werror",
         str(ROOT / "benchmarks/task-footprint.c"), "-o", binary],
        check=True,
    )
    for invalid in ("0", "-1", "abc", "2147483648"):
        assert subprocess.run([binary, invalid], capture_output=True).returncode != 0
    child = subprocess.Popen(
        [sys.executable, "-u", "-c", """
print('ready')
input()
ballast = bytearray(64 << 20)
for index in range(0, len(ballast), 4096):
    ballast[index] = 1
print('allocated')
input()
"""], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
    )
    try:
        assert child.stdout.readline().strip() == "ready"
        before = int(subprocess.check_output([binary, str(child.pid)], timeout=5))
        child.stdin.write("allocate\n")
        child.stdin.flush()
        assert child.stdout.readline().strip() == "allocated"
        after = int(subprocess.check_output([binary, str(child.pid)], timeout=5))
        assert before > 0 and after - before >= 48 << 20, (before, after)
        child.stdin.write("done\n")
        child.stdin.flush()
        child.wait(timeout=5)
        assert subprocess.run([binary, str(child.pid)], capture_output=True).returncode != 0
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
print("PASS: task ledger charges a child's allocation; invalid and exited tasks fail")
