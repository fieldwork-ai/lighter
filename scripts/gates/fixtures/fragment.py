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
freed. Memory is then available but in pieces. Prints `fragmented <free
256 KiB blocks> <slab MiB>` and holds it all until killed. Pinning again
whenever the guest freed a block kept the guest short enough that the OOM
killer took the containers a gate was watching.
"""

import ctypes
import errno
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
    # Memory running out first is the same answer, sooner.
    try:
        while meminfo("MemFree:") > total // 6:
            path = f"{ROOT}/{directory}"
            os.mkdir(path)
            for i in range(20000):
                name = f"{path}/{i:05d}-{'x' * 40}"
                os.close(os.open(name, os.O_CREAT | os.O_WRONLY, 0o644))
                names.append(name)
            directory += 1
    except OSError as e:
        if e.errno != errno.ENOMEM:
            raise
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


def pins(held, margin, lower_reserve):
    """One page of every physical block of nearly all free memory, pinned.
    `margin` is left free for everything else in the guest; the kernel's
    reserve is halved for the first pass only. Taking all but 24 MiB with
    the reserve at 4 MiB, a pass ran the guest out of memory and the OOM
    killer took another container's server."""
    sysctl("compact_unevictable_allowed", 0)
    reserve = sysctl("min_free_kbytes")
    if lower_reserve:
        sysctl("min_free_kbytes", max(int(reserve) // 2, 8192))
    try:
        for _ in range(4):
            size = (meminfo("MemFree:") - margin) // PAGE * PAGE
            if size < (16 << 20) or free_blocks() < 16:
                break
            try:
                region = mmap.mmap(-1, size, mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS | 0x8000)
            except OSError as e:
                if e.errno != errno.ENOMEM:
                    raise
                break
            base = ctypes.addressof(ctypes.c_char.from_buffer(region))
            if libc.mlock(base, size) != 0:
                region.close()
                break
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


# If the guest runs out regardless, this is what goes, never a container
# the gate is watching.
open("/proc/self/oom_score_adj", "w").write("1000")
held = []
slab()
pins(held, 64 << 20, True)
time.sleep(1)
pins(held, 64 << 20, True)
# Where the allocator places a pass's pages is the guest's choice, and two
# passes left 318 free blocks once where 29 is usual (2026-10-20, the M1). A
# pass pins one page in 64 of what it maps, so a few more cost little: a
# bounded number, until the guest is below the gate's line of 128 with room
# to spare, and never a loop that follows the guest as it frees.
for _ in range(4):
    if free_blocks() < 96:
        break
    time.sleep(1)
    pins(held, 64 << 20, False)
print("fragmented", free_blocks(), meminfo("Slab:") >> 20, flush=True)
while True:
    time.sleep(3600)
