#!/usr/bin/env python3
"""Remove only a temporary hardware test's app registrations before cleanup."""

import argparse
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("work", type=Path)
work = parser.parse_args().work.resolve()
if work.parent != Path("/tmp").resolve() or not work.name.startswith("lighter-"):
    raise SystemExit("refusing to unregister outside a /tmp/lighter-* test home")
registrar = Path(
    "/System/Library/Frameworks/CoreServices.framework/Frameworks/"
    "LaunchServices.framework/Support/lsregister"
)
if not registrar.is_file():
    raise SystemExit("Launch Services registration tool is unavailable")
count = 0
failures = 0
for app in work.rglob("lighter.app"):
    if app.is_dir() and app.resolve().is_relative_to(work):
        result = subprocess.run([str(registrar), "-u", str(app)], check=False)
        failures += result.returncode != 0
        count += 1
print(f"Temporary test bundle cleanup: {count} paths, {failures} errors")
raise SystemExit(bool(failures))
