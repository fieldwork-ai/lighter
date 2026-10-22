#!/usr/bin/env python3
"""Write a release's THIRD-PARTY-NOTICES, or check what it is written from.

    scripts/notices.py <version> <output> [--rootfs guest/out/rootfs.ext4]
    scripts/notices.py --check

A release ships three kinds of other people's code: the Rust crates built into
the Mac binary and the guest's own programs (found by cargo-about, under the
policy in licenses/about.toml), the C and C++ libraries linked beside them
(licenses/native.json, texts in licenses/native/), and the guest: Linux and the
Alpine packages of its root filesystem, read from the image itself. Their
source, with every package's own copyright notices, is the release's
lighter-<version>-guest-source.tar (scripts/guest-source.sh).

--check is what CI runs: the crates' licences against the policy, and each
native component's version against the build script that pins it.
"""
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
LICENSES = ROOT / "licenses"
REPOSITORY = "https://github.com/fieldwork-ai/lighter"

# The Rust code a release ships: the Mac binary's workspace, and the guest's
# agent and ONNX Runtime plugin, which are built apart from it.
MANIFESTS = [
    ("host", ROOT / "Cargo.toml"),
    ("guest", ROOT / "guest/agent/Cargo.toml"),
    ("guest", ROOT / "guest/ane-ep/Cargo.toml"),
]

# The copyleft licences in the guest, whose terms require their text to go
# with the program. Permissive licences require the copyright notice itself,
# which is in each package's source in the guest source archive.
COPYLEFT_TEXTS = {
    "GPL-1.0": "GPL-1.0-only.txt",
    "GPL-2.0": "GPL-2.0-only.txt",
    "GPL-3.0": "GPL-3.0-only.txt",
    "LGPL-2.0": "LGPL-2.0-only.txt",
    "LGPL-2.1": "LGPL-2.1-only.txt",
    "LGPL-3.0": "LGPL-3.0-only.txt",
    "MPL-2.0": "MPL-2.0.txt",
}


def fail(message):
    sys.exit(f"error: {message}")


def load_components():
    return json.loads((LICENSES / "native.json").read_text())["components"]


def pinned_version(pin):
    text = (ROOT / pin["file"]).read_text()
    match = re.search(r"\$\{%s:-([^}]+)\}" % re.escape(pin["var"]), text)
    if not match:
        fail(f"{pin['file']} has no ${{{pin['var']}:-...}} pin")
    return match.group(1)


def check_native(components):
    for component in components:
        pin = component["pin"]
        if "file" in pin and pinned_version(pin) != component["version"]:
            fail(f"{pin['file']} pins {component['name']} at {pinned_version(pin)}, licenses/native.json says "
                 f"{component['version']}: update it and run scripts/fetch-native-licenses.py")
        for entry in component["files"]:
            if not (LICENSES / "native" / entry["path"]).is_file():
                fail(f"licenses/native/{entry['path']} is missing: run scripts/fetch-native-licenses.py")


def cargo_about(manifest):
    binary = shutil.which("cargo-about") or str(pathlib.Path.home() / ".cargo/bin/cargo-about")
    if not os.access(binary, os.X_OK):
        fail("cargo-about is not installed: cargo install --locked cargo-about --features cli")
    result = subprocess.run(
        [binary, "about", "generate", "--fail", "-c", str(LICENSES / "about.toml"),
         "--manifest-path", str(manifest), "--format", "json"],
        capture_output=True, text=True)
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        fail(f"cargo-about refused {manifest.relative_to(ROOT)}: a crate's licence is missing or not in licenses/about.toml")
    return json.loads(result.stdout)


def ours(crate):
    """lighter's own crates are under its own licence, not a third party's."""
    path = pathlib.Path(crate["manifest_path"]).resolve()
    return any(parent in path.parents for parent in (ROOT / "crates", ROOT / "guest"))


def crate_licences(part):
    """Each distinct licence text, with the third-party crates under it."""
    texts = {}
    for manifest_part, manifest in MANIFESTS:
        if manifest_part != part:
            continue
        for licence in cargo_about(manifest)["licenses"]:
            crates = {f"{u['crate']['name']} {u['crate']['version']}" for u in licence["used_by"] if not ours(u["crate"])}
            if crates:
                entry = texts.setdefault(licence["text"].strip(), {"id": licence["id"], "crates": set()})
                entry["crates"] |= crates
    return sorted(texts.items(), key=lambda item: (item[1]["id"], sorted(item[1]["crates"])[0]))


def debugfs():
    for candidate in (os.environ.get("DEBUGFS"), shutil.which("debugfs"),
                      "/opt/homebrew/opt/e2fsprogs/sbin/debugfs", "/usr/local/opt/e2fsprogs/sbin/debugfs"):
        if candidate and os.access(candidate, os.X_OK):
            return candidate
    fail("debugfs is needed to read the guest's root filesystem: brew install e2fsprogs")


def read_guest_file(rootfs, path):
    result = subprocess.run([debugfs(), "-R", f"cat {path}", str(rootfs)], capture_output=True, text=True)
    if result.returncode != 0 or not result.stdout:
        fail(f"could not read {path} from {rootfs}: {result.stderr.strip()}")
    return result.stdout


def alpine_packages(rootfs):
    """The guest's packages, from apk's own database in the shipped image."""
    packages, current = [], {}
    for line in read_guest_file(rootfs, "/lib/apk/db/installed").splitlines() + [""]:
        if not line:
            if current:
                packages.append(current)
            current = {}
            continue
        key, _, value = line.partition(":")
        if key in ("P", "V", "L", "o", "c") and key not in current:
            current[key] = value
    if not packages:
        fail(f"no packages in {rootfs}'s apk database")
    return read_guest_file(rootfs, "/etc/alpine-release").strip(), sorted(packages, key=lambda p: p["P"])


def copyleft_in(expression):
    found = set()
    for family in COPYLEFT_TEXTS:
        if re.search(r"(?<![\w-])" + re.escape(family) + r"(?:-only|-or-later)?(?![\w.])", expression):
            found.add(family)
    return found


def rule(title, char="="):
    return f"{title}\n{char * len(title)}\n"


def write_notices(version, output, rootfs):
    components = load_components()
    check_native(components)
    kernel = (ROOT / "guest/out/kernel.version").read_text().strip()
    alpine, packages = alpine_packages(rootfs)
    source = f"lighter-{version}-guest-source.tar"
    release = f"{REPOSITORY}/releases/tag/v{version}"
    out = []
    out.append(rule(f"Third-party notices for lighter {version}"))
    out.append(
        "lighter is Copyright 2026 Fieldwork AI Inc. and is licensed under the MIT License (LICENSE-MIT) or the "
        "Apache License, Version 2.0 (LICENSE-APACHE), at your option. A release is built with, and ships, the "
        "third-party software below, each under its own licence.\n\n"
        f"The source of the Linux guest (Linux with lighter's patches and configuration, and every Alpine package "
        f"in its root filesystem, with their copyright notices and licences) is {source}, published with this "
        f"release at {release}. lighter's own source is {REPOSITORY} at the tag v{version}.\n")

    out.append(rule("1. The lighter program for macOS (bin/lighter, share/lighter/lighter.app)"))
    out.append(rule("1.1 C and C++ libraries", "-"))
    for component in [c for c in components if c["part"] == "host"]:
        within = f", built into {component['within']}" if "within" in component else ""
        out.append(f"{component['name']} {component['version']} ({component['url']}{within}), {component['license']}\n")
        for entry in component["files"]:
            out.append(f"--- {entry['path']} ---\n\n{(LICENSES / 'native' / entry['path']).read_text().strip()}\n")
    out.append(rule("1.2 Rust crates", "-"))
    for text, entry in crate_licences("host"):
        out.append(f"{entry['id']}, used by: {', '.join(sorted(entry['crates']))}\n\n{text}\n")

    out.append(rule("2. The Linux guest (share/lighter/Image, share/lighter/rootfs.ext4)"))
    out.append(rule("2.1 Linux", "-"))
    out.append(
        f"The guest kernel is Linux {kernel} (https://kernel.org), licensed under GPL-2.0-only WITH "
        f"Linux-syscall-note, with lighter's patches (guest/kernel/patches, under the same licence as the files "
        f"they change) and configuration. Its complete corresponding source is in {source}. The GNU General Public "
        f"License, version 2, is reproduced in section 3.\n")
    out.append(rule(f"2.2 Alpine Linux {alpine} packages", "-"))
    out.append(
        f"The root filesystem is built from these Alpine Linux packages (https://alpinelinux.org). Each package's "
        f"source, as Alpine built it (the upstream source, Alpine's patches and its APKBUILD, at the aports commit "
        f"the package records), is in {source}, and carries the package's copyright notices and licence texts.\n")
    name_width = max(len(p["P"]) for p in packages)
    version_width = max(len(p["V"]) for p in packages)
    for package in packages:
        out.append(f"  {package['P'].ljust(name_width)}  {package['V'].ljust(version_width)}  {package['L']}")
    out.append("")
    out.append(rule("2.3 lighter's programs in the guest: Rust crates", "-"))
    for text, entry in crate_licences("guest"):
        out.append(f"{entry['id']}, used by: {', '.join(sorted(entry['crates']))}\n\n{text}\n")
    out.append(rule("2.4 lighter-mps wheels", "-"))
    for component in [c for c in components if c["part"] == "guest"]:
        out.append(f"{component['name']} {component['version']} ({component['url']}), {component['license']}\n")
        for entry in component["files"]:
            out.append(f"--- {entry['path']} ---\n\n{(LICENSES / 'native' / entry['path']).read_text().strip()}\n")

    out.append(rule("3. Licence texts the guest's copyleft software requires"))
    families = {"GPL-2.0"} | set().union(*(copyleft_in(p["L"]) for p in packages))
    for family in sorted(families):
        path = LICENSES / "spdx" / COPYLEFT_TEXTS[family]
        if not path.is_file():
            fail(f"licenses/spdx/{COPYLEFT_TEXTS[family]} is missing: run scripts/fetch-native-licenses.py")
        out.append(f"--- {family} ---\n\n{path.read_text().strip()}\n")

    pathlib.Path(output).write_text("\n".join(out) + "\n")
    print(f"{output}: {len(packages)} Alpine packages, licences {', '.join(sorted(families))}")


def main(argv):
    if argv == ["--check"]:
        check_native(load_components())
        for _, manifest in MANIFESTS:
            crates = {u["crate"]["name"] for l in cargo_about(manifest)["licenses"] for u in l["used_by"]}
            print(f"{manifest.relative_to(ROOT)}: {len(crates)} crates, every licence accepted")
        return
    args = list(argv)
    rootfs = ROOT / "guest/out/rootfs.ext4"
    if "--rootfs" in args:
        i = args.index("--rootfs")
        rootfs = pathlib.Path(args[i + 1])
        del args[i:i + 2]
    if len(args) != 2:
        sys.exit(__doc__)
    write_notices(args[0], args[1], rootfs)


if __name__ == "__main__":
    main(sys.argv[1:])
