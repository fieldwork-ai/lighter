#!/usr/bin/env python3
"""Check virtio-mem's offline_size with the actual notifier size and page-array lookup."""
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
patches = root / 'guest/kernel/patches'


def added(patch):
    return '\n'.join(line[1:] for line in patch.read_text().splitlines()
                     if line.startswith((' ', '+')) and not line.startswith('+++'))


def function(patch, name):
    match = re.search(r'^static unsigned long ' + name + r'\([^;]*?\)\n\{.*?^\}',
                      added(patch), re.M | re.S)
    if not match:
        raise SystemExit(f'Cannot find {name} in {patch.name}')
    return match.group()


def accounted(patch, notify, op):
    # What the notifier hands atomic64_<op> for offline_size right after the
    # SBM bookkeeping of that notification: the call site, not the helper.
    match = re.search(re.escape(notify) + r'\(vm, id[^;]*\);\s*atomic64_' + op +
                      r'\((.*?), &vm->offline_size\);', added(patch), re.S)
    if not match:
        raise SystemExit(f'Cannot find the {notify} accounting in {patch.name}')
    return match.group(1)


notified = (Path(sys.argv[1]) if len(sys.argv) > 1 else
            patches / '0059-virtio-mem-offline-size-counts-the-page-array.patch')
vmemmap = patches / '0024-virtio-mem-memmap-on-memory.patch'
with tempfile.TemporaryDirectory(prefix='lighter-virtio-mem-') as temp:
    source = Path(temp) / 'offline-size.c'
    binary = Path(temp) / 'offline-size'
    source.write_text((root / 'guest/kernel/tests/virtio-mem-offline-size.c').read_text()
                      .replace('@VMEMMAP_FUNCTION@', function(vmemmap, 'virtio_mem_vmemmap_pages'))
                      .replace('@NOTIFIED_FUNCTION@', function(notified, 'virtio_mem_notified_size'))
                      .replace('@ONLINE_SIZE@', accounted(notified, 'virtio_mem_sbm_notify_online', 'sub'))
                      .replace('@OFFLINE_SIZE@', accounted(notified, 'virtio_mem_sbm_notify_offline', 'add')))
    subprocess.run(shlex.split(os.environ.get('CC', 'cc')) + [
        '-std=c11', '-Wall', '-Wextra', '-Werror', '-Wno-unused-function', str(source), '-o', str(binary)
    ], check=True)
    subprocess.run([str(binary)], check=True)
