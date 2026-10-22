#!/usr/bin/env python3
"""Fetch the licence texts of the C and C++ code a release ships.

    scripts/fetch-native-licenses.py

Reads licenses/native.json and writes each component's files into
licenses/native/ at the version its build script pins, and the copyleft
licence texts the guest's notices reproduce into licenses/spdx/, from SPDX's
licence list at SPDX_LIST. Run it when a pin moves (scripts/notices.py
refuses to write notices until it has). A component built into another
(`within`) is checked against the revision its parent records, so a MoltenVK
bump cannot leave SPIRV-Cross's text behind.
"""
import json
import pathlib
import sys
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
from notices import COPYLEFT_TEXTS, load_components, pinned_version  # noqa: E402

SPDX_LIST = "v3.27.0"


def fetch(url):
    with urllib.request.urlopen(url, timeout=60) as response:
        return response.read()


def main():
    components = load_components()
    by_name = {c["name"]: c for c in components}
    for component in components:
        pin = component["pin"]
        if "within" in pin:
            parent = by_name[pin["within"]]
            base = parent["files"][0]["url"].rsplit("/", 1)[0].format(version=parent["version"])
            recorded = fetch(f"{base}/{pin['revision']}").decode().strip()
            if recorded != component["version"]:
                sys.exit(f"error: {parent['name']} {parent['version']} builds {component['name']} at {recorded}, "
                         f"licenses/native.json says {component['version']}")
        else:
            version = pinned_version(pin)
            if version != component["version"]:
                sys.exit(f"error: {pin['file']} pins {component['name']} at {version}, "
                         f"licenses/native.json says {component['version']}")
        for entry in component["files"]:
            target = ROOT / "licenses" / "native" / entry["path"]
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(fetch(entry["url"].format(version=component["version"])))
            print(f"{component['name']} {component['version']}: {entry['path']}")
    for name in sorted(set(COPYLEFT_TEXTS.values())):
        target = ROOT / "licenses" / "spdx" / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(fetch(f"https://raw.githubusercontent.com/spdx/license-list-data/{SPDX_LIST}/text/{name}"))
        print(f"SPDX {SPDX_LIST}: {name}")


if __name__ == "__main__":
    main()
