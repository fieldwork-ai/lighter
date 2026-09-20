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
new_side = "\n".join(
    line[1:] for line in patch.read_text().splitlines()
    if line.startswith(("+", " ")) and not line.startswith("+++")
)
arch = re.search(
    r"^void __cpuidle arch_cpu_idle\(void\)\n\{.*?^\}",
    new_side, re.M | re.S,
)
if not arch:
    sys.exit("Cannot find arch_cpu_idle in the kernel patch")
def added_function(pattern, what):
    found = re.search(pattern, added, re.M | re.S)
    if not found:
        sys.exit(f"Cannot find {what} in the kernel patch")
    return found.group()

adjust = added_function(
    r"^static void __cpuidle idle_poll_adjust\(u64 block_ns, bool timer_due\)\n\{.*?^\}",
    "idle_poll_adjust",
)
timer_due = added_function(
    r"^static bool __cpuidle idle_timer_due\(u64 next_timer_ns\)\n\{.*?^\}",
    "idle_timer_due",
)
slack = added_function(r"^#define IDLE_TIMER_SLACK_NS\t\d+$", "IDLE_TIMER_SLACK_NS")
template = (root / "guest/kernel/tests/idle-poll.c").read_text()
with tempfile.TemporaryDirectory(prefix="lighter-idle-poll-") as temp:
    source = Path(temp) / "idle-poll.c"
    binary = Path(temp) / "idle-poll"
    source.write_text(
        template.replace("@IDLE_POLL@", match.group())
        .replace("@ARCH_IDLE@", arch.group())
        .replace("@IDLE_ADJUST@", adjust)
        .replace("@IDLE_TIMER_DUE@", timer_due)
        .replace("@IDLE_SLACK@", slack)
    )
    subprocess.run(
        shlex.split(os.environ.get("CC", "cc")) + [
            "-std=c11", "-Wall", "-Wextra", "-Werror", "-Wno-unused-function",
            str(source), "-o", str(binary),
        ],
        check=True,
    )
    sys.exit(subprocess.run([str(binary)]).returncode)
