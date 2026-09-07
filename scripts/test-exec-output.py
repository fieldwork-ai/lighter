#!/usr/bin/env python3
"""Check short Docker exec output against an explicitly selected engine.

Example (container must already exist and provide printf):
  DOCKER_HOST=unix:///path/to/docker.sock python3 scripts/test-exec-output.py \
    --container test-container --reps 100 --output /tmp/exec-results.json

Does not create, stop or remove containers. A missing response with exit status
zero is a failure, not a retry. Each invocation has a bounded timeout.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--container", required=True)
    parser.add_argument("--reps", type=int, default=100)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not os.environ.get("DOCKER_HOST") or os.environ.get("DOCKER_CONTEXT"):
        parser.error("set DOCKER_HOST explicitly and unset DOCKER_CONTEXT")
    if args.reps < 1:
        parser.error("--reps must be positive")
    if args.output.exists():
        parser.error("output already exists; choose a new path")

    results = []
    for iteration in range(args.reps):
        expected = f"lighter-exec-{iteration:06d}"
        try:
            process = subprocess.run(
                ["docker", "exec", args.container, "printf", "%s", expected],
                capture_output=True, text=True, timeout=15,
            )
            row = dict(iteration=iteration, returncode=process.returncode,
                       expected=expected, stdout=process.stdout,
                       stderr=process.stderr,
                       passed=process.returncode == 0 and process.stdout == expected)
        except subprocess.TimeoutExpired:
            row = dict(iteration=iteration, passed=False, error="15-second timeout")
        results.append(row)
        args.output.write_text(json.dumps(results, indent=2) + "\n")
        if not row["passed"]:
            print(json.dumps(row), flush=True)

    failures = sum(not row["passed"] for row in results)
    print(f"{len(results)} exec commands; {failures} failures")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
