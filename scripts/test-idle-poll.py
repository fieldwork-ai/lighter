#!/usr/bin/env python3
"""Compile the patched idle loop with deterministic scheduler interleavings."""
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
patch = (
    Path(sys.argv[1]) if len(sys.argv) > 1
    else root / "guest/kernel/patches/0011-arm64-idle-poll.patch"
)
added = "\n".join(
    line[1:] for line in patch.read_text().splitlines()
    if line.startswith("+") and not line.startswith("+++")
)
match = re.search(
    r"^static bool __cpuidle idle_poll\(u64 \*start\)\n\{.*?^\}",
    added, re.M | re.S,
)
if not match:
    sys.exit("Cannot find idle_poll in the kernel patch")
template = (root / "guest/kernel/tests/idle-poll.c").read_text()
with tempfile.TemporaryDirectory(prefix="lighter-idle-poll-") as temp:
    source = Path(temp) / "idle-poll.c"
    binary = Path(temp) / "idle-poll"
    source.write_text(template.replace("@IDLE_POLL@", match.group()))
    subprocess.run(
        shlex.split(os.environ.get("CC", "cc")) + [
            "-std=c11", "-Wall", "-Wextra", "-Werror", "-Wno-unused-function",
            str(source), "-o", str(binary),
        ],
        check=True,
    )
    sys.exit(subprocess.run([str(binary)]).returncode)
