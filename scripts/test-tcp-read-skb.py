#!/usr/bin/env python3
"""Check the actual patched tcp_read_skb hands each byte to its reader once."""
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
patch = (Path(sys.argv[1]) if len(sys.argv) > 1 else
         root / 'guest/kernel/patches/0030-sockmap-overlap-read-once.patch')
text = '\n'.join(line[1:] for line in patch.read_text().splitlines()
                 if line.startswith((' ', '+')) and not line.startswith('+++'))
match = re.search(r'^int tcp_read_skb\(.*?^\}', text, re.M | re.S)
if not match:
    raise SystemExit('Cannot find tcp_read_skb in patch')
with tempfile.TemporaryDirectory(prefix='lighter-tcp-read-skb-') as temp:
    source = Path(temp) / 'read.c'
    binary = Path(temp) / 'read'
    source.write_text((root / 'guest/kernel/tests/tcp-read-skb.c').read_text()
                      .replace('@READ_FUNCTION@', match.group()))
    subprocess.run(shlex.split(os.environ.get('CC', 'cc')) + [
        '-std=c11', '-Wall', '-Wextra', '-Werror', '-Wno-unused-function', str(source), '-o', str(binary)
    ], check=True)
    subprocess.run([str(binary)], check=True)
