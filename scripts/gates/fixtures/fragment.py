"""Fragments a guest's memory the way a media server's library scan does.

Run in a privileged container. What fragments a long-running guest is
filesystem metadata: inodes and dentries live in slab pages, which neither
compaction nor reclaim can move while their files exist. So, first, files
on a tmpfs until the slab holds most of the guest, then all but one in
`KEEP` deleted at random: most slab pages empty and go back, and the tenth
or so still holding a file are scattered through memory, each pinning its
256 KiB block. Second, whatever free memory the slab never reached is
pinned the same way, one page of every physical 64-page block (from
/proc/self/pagemap), mlocked where compaction may not move it, the rest
freed. Memory is then available but in pieces, and is kept so: any whole
block the guest frees later is pinned in its turn. Prints `fragmented
<free 256 KiB blocks> <slab MiB>` and holds it all until killed.
"""

import ctypes
import mmap
import os
import random
import struct
import subprocess
import time

KEEP = 200
ROOT = "/frag"
PAGE = 4096
BLOCK = 64  # pages: 256 KiB

libc = ctypes.CDLL(None, use_errno=True)
libc.mlock.argtypes = libc.munlock.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
libc.madvise.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]


def meminfo(name):
    for line in open("/proc/meminfo"):
        if line.startswith(name):
            return int(line.split()[1]) * 1024
    return 0


def free_blocks():
    total = 0
    for line in open("/proc/buddyinfo"):
        counts = line.split("zone")[1].split()[1:]
        total += sum(int(n) << (order - 6) for order, n in enumerate(counts) if order >= 6)
    return total


def sysctl(name, value=None):
    path = "/proc/sys/vm/" + name
    old = open(path).read().strip()
    if value is not None:
        open(path, "w").write(str(value))
    return old


def slab():
    os.makedirs(ROOT, exist_ok=True)
    subprocess.run(["mount", "-t", "tmpfs", "-o", "size=100%,nr_inodes=0", "tmpfs", ROOT], check=True)
    total = meminfo("MemTotal:")
    names = []
    directory = 0
    # Free, not available: dentries are reclaimable slab, which available
    # counts, and a loop on it grew the slab until the OOM killer came.
    while meminfo("MemFree:") > total // 6:
        path = f"{ROOT}/{directory}"
        os.mkdir(path)
        for i in range(20000):
            name = f"{path}/{i:05d}-{'x' * 40}"
            os.close(os.open(name, os.O_CREAT | os.O_WRONLY, 0o644))
            names.append(name)
        directory += 1
    random.seed(84)
    random.shuffle(names)
    for name in names[len(names) // KEEP :]:
        os.unlink(name)
    open("/proc/sys/vm/drop_caches", "w").write("2")


def release(base, first, count):
    if count:
        address = base + first * PAGE
        libc.munlock(address, count * PAGE)
        libc.madvise(address, count * PAGE, 4)


def pins(held):
    sysctl("compact_unevictable_allowed", 0)
    reserve = sysctl("min_free_kbytes", 4096)
    try:
        for _ in range(4):
            size = (meminfo("MemFree:") - (24 << 20)) // PAGE * PAGE
            if size < (16 << 20) or free_blocks() < 16:
                break
            region = mmap.mmap(-1, size, mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS | 0x8000)
            base = ctypes.addressof(ctypes.c_char.from_buffer(region))
            if libc.mlock(base, size) != 0:
                raise OSError(ctypes.get_errno(), "mlock")
            pages = size // PAGE
            with open("/proc/self/pagemap", "rb") as pagemap:
                pagemap.seek(base // PAGE * 8)
                entries = pagemap.read(pages * 8)
            seen = set()
            run_start, run = 0, 0
            for i in range(pages):
                (entry,) = struct.unpack_from("<Q", entries, i * 8)
                block = (entry & ((1 << 55) - 1)) // BLOCK
                if entry >> 63 and block not in seen:
                    seen.add(block)
                    release(base, run_start, run)
                    run_start, run = i + 1, 0
                else:
                    run += 1
            release(base, run_start, run)
            held.append(region)
    finally:
        sysctl("min_free_kbytes", reserve)


held = []
slab()
pins(held)
time.sleep(1)
pins(held)
print("fragmented", free_blocks(), meminfo("Slab:") >> 20, flush=True)
# And kept so: whatever the guest frees in whole blocks from here on (a
# cache dropped, a buffer given back) is pinned in its turn.
while True:
    time.sleep(1)
    if free_blocks() >= 8:
        pins(held)
