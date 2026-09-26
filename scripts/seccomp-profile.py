#!/usr/bin/env python3
"""The guest's seccomp profile: Docker's own default, with Podman's rule for the
NUMA memory-policy calls.

Docker allows `get_mempolicy`, `set_mempolicy` and `mbind` only to a container
holding CAP_SYS_NICE. libnuma probes with `get_mempolicy`, and x265 then
allocates no thread pool at all: an HEVC encode took 3.5 s on 8 vCPUs where it
takes 1.3 s with the calls allowed (M5, 2026-09-26). Podman allows the three to
every container (containers/common's profile). They act only on the caller's
own memory; `migrate_pages`, `move_pages` and `set_mempolicy_home_node` stay as
Docker has them, and the kernel still requires CAP_SYS_NICE to move pages
other processes share.

    scripts/seccomp-profile.py           write guest/rootfs/seccomp.json
    scripts/seccomp-profile.py --check   fail unless it is exactly that
"""
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent / "guest" / "rootfs"
UPSTREAM = ROOT / "seccomp-docker-28.3.3.json"
PROFILE = ROOT / "seccomp.json"
ALLOWED = ["get_mempolicy", "set_mempolicy", "mbind"]


def build():
    profile = json.loads(UPSTREAM.read_text())
    moved = 0
    for rule in profile["syscalls"]:
        if rule.get("includes", {}).get("caps") == ["CAP_SYS_NICE"] and set(ALLOWED) <= set(rule["names"]):
            rule["names"] = [n for n in rule["names"] if n not in ALLOWED]
            moved += 1
    if moved != 1:
        sys.exit(f"expected one CAP_SYS_NICE rule holding {ALLOWED} upstream, found {moved}")
    profile["syscalls"].append({"names": ALLOWED, "action": "SCMP_ACT_ALLOW"})
    return json.dumps(profile, indent="\t") + "\n"


if __name__ == "__main__":
    text = build()
    if "--check" in sys.argv[1:]:
        if PROFILE.read_text() != text:
            sys.exit(f"{PROFILE} is not Docker 28.3.3's default with the mempolicy rule; run scripts/seccomp-profile.py")
        print("seccomp profile: Docker 28.3.3's default, mempolicy calls allowed")
    else:
        PROFILE.write_text(text)
