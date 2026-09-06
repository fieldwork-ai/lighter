#!/usr/bin/env python3
"""Check the actual patched vsock wait helper's socket-lock contract."""
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
patch = (Path(sys.argv[1]) if len(sys.argv) > 1 else
         root / 'guest/kernel/patches/0027-vsock-bpf-unlock-while-waiting.patch')
text = '\n'.join(line[1:] for line in patch.read_text().splitlines()
                 if line.startswith((' ', '+')) and not line.startswith('+++'))
match = re.search(r'^static bool vsock_msg_wait_data\(.*?^\}', text, re.M | re.S)
if not match:
    raise SystemExit('Cannot find vsock_msg_wait_data in patch')
with tempfile.TemporaryDirectory(prefix='lighter-vsock-wait-') as temp:
    source = Path(temp) / 'wait.c'
    binary = Path(temp) / 'wait'
    source.write_text((root / 'guest/kernel/tests/vsock-wait.c').read_text()
                      .replace('@WAIT_FUNCTION@', match.group()))
    subprocess.run(shlex.split(os.environ.get('CC', 'cc')) + [
        '-std=c11', '-Wall', '-Wextra', '-Werror', '-Wno-unused-function', str(source), '-o', str(binary)
    ], check=True)
    subprocess.run([str(binary)], check=True)
