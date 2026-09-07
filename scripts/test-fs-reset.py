#!/usr/bin/env python3
"""Prove cache-loss recovery for names, open files and existing mmaps.

The benchmark VMM's test writer mutates files in its own process, so FSEvents
IgnoreSelf suppresses individual notifications. A control phase first proves
that the guest still has stale data; only then is a reset or overflow injected.
"""
import argparse
import os
from pathlib import Path
import select
import subprocess
import tempfile
import time

GUEST = r'''
import mmap, os, resource, signal, sys, time
from pathlib import Path
p = Path('/work')
f = open(p/'content', 'rb', buffering=0)
mf = open(p/'mapped', 'rb', buffering=0)
mm = mmap.mmap(mf.fileno(), 0, access=mmap.ACCESS_READ)
tf = open(p/'truncated', 'rb', buffering=0)
tm = mmap.mmap(tf.fileno(), 0, access=mmap.ACCESS_READ)
resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
old_mtime = (p/'content').stat().st_mtime_ns

def check(new):
    f.seek(0)
    assert f.read() == (b'after!!' if new else b'before!'), 'open descriptor'
    assert (p/'content').read_bytes() == (b'after!!' if new else b'before!'), 'path contents'
    assert mm[:] == (b'mappedB' if new else b'mappedA'), 'existing mmap'
    assert (p/'empty').read_bytes() == (b'filled' if new else b''), 'empty-file attributes'
    assert (p/'missing').exists() == new, 'negative dentry'
    assert (p/'gone').exists() != new, 'deleted dentry'
    assert (p/'renamed').exists() != new, 'rename source'
    assert (p/'renamed-new').exists() == new, 'rename destination'
    assert sorted(os.listdir(p/'dir')) == (['new'] if new else []), 'directory listing'
    assert (p/'content').stat().st_mtime_ns == old_mtime, 'same-mtime fixture'
    if not new:
        assert tm[0] == ord('x'), 'mapped truncation precondition'

def check_truncated_mapping():
    child = os.fork()
    if child == 0:
        value = tm[0]
        os._exit(1)
    _, status = os.waitpid(child, 0)
    assert os.WIFSIGNALED(status) and os.WTERMSIG(status) == signal.SIGBUS, 'host-truncated mmap must SIGBUS'

check(False)
print('READY', flush=True)
for command in sys.stdin:
    command = command.strip()
    try:
        if command == 'old':
            end = time.monotonic() + 1
            while time.monotonic() < end:
                check(False)
                time.sleep(.01)
        elif command == 'new':
            end = time.monotonic() + 5
            while True:
                try:
                    check(True)
                    check_truncated_mapping()
                    break
                except (AssertionError, FileNotFoundError):
                    if time.monotonic() >= end:
                        raise
                    time.sleep(.01)
        else:
            raise AssertionError('unknown command')
        print('PASS ' + command, flush=True)
    except Exception as exc:
        print('FAIL ' + command + ': ' + str(exc), flush=True)
        sys.exit(1)
'''


def run(*args, **kwargs):
    return subprocess.run(args, check=True, timeout=kwargs.pop('timeout', 60), **kwargs)


def line(proc, timeout=15):
    if not select.select([proc.stdout], [], [], timeout)[0]:
        raise RuntimeError('guest probe did not answer')
    answer = proc.stdout.readline().strip()
    if not answer:
        raise RuntimeError('guest probe exited before answering')
    return answer


def wait_for(predicate, limit=60):
    deadline = time.monotonic() + limit
    while not predicate():
        if time.monotonic() >= deadline:
            raise RuntimeError('probe timed out')
        time.sleep(.02)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin', default='target/release/examples/lighter-bench')
    parser.add_argument('--kernel', default='guest/out/Image')
    parser.add_argument('--log-dir', default='.logs/fs-reset')
    args = parser.parse_args()
    logs = Path(args.log_dir).resolve()
    logs.mkdir(parents=True, exist_ok=True)
    for mode in ['reset', 'overflow']:
        # Under HOME: the host share and all data belong to this probe.
        with tempfile.TemporaryDirectory(prefix='lighter-reset-', dir=Path.home()) as tmp:
            root = Path(tmp)
            share = root/'share'
            share.mkdir()
            (share/'dir').mkdir()
            for name, contents in [('content','before!'), ('mapped','mappedA'),
                                   ('empty',''), ('gone','gone'), ('renamed','rename'),
                                   ('truncated', 'x' * 4096)]:
                (share/name).write_text(contents)
            (share/'probe.py').write_text(GUEST)
            run('cp', '-c', 'guest/out/rootfs.ext4', str(root/'rootfs.ext4'))
            env = dict(os.environ, LIGHTER_TEST_FS_RESET=str(share))
            socket = root/'docker.sock'
            docker = ['docker', '-H', f'unix://{socket}']
            command = [args.bin, '--kernel', args.kernel, '--disk', str(root/'rootfs.ext4'),
                       '--disk', str(root/'data.img'), '--disk-size-gib', '32', '--net',
                       '--run-dir', str(root), '--vsock', f'{socket}:2375',
                       '--share', f'probe:{share}', '--no-tty', '--cpus', '4',
                       '--memory-mib', '2048', '--cmdline',
                       f'console=ttyAMA0 panic=-1 root=/dev/vda rw init=/sbin/lighter-init '
                       f'lighter.time={int(time.time())} lighter.share=probe:/mnt/probe']
            guest = None
            with (logs/f'{mode}-boot.log').open('w') as boot:
                vm = subprocess.Popen(command, env=env, stdout=boot, stderr=subprocess.STDOUT)
                try:
                    def ready():
                        if vm.poll() is not None:
                            raise RuntimeError('VMM exited during probe boot')
                        return socket.exists() and 'AGENT listening' in (logs/f'{mode}-boot.log').read_text(errors='replace')
                    wait_for(ready)
                    with (logs/f'{mode}-pull.log').open('w') as pull:
                        run(*docker, 'pull', 'python:3.13-alpine', stdout=pull, stderr=subprocess.STDOUT, timeout=180)
                    with (logs/f'{mode}-guest.err').open('w') as err:
                        guest = subprocess.Popen(docker + ['run', '--rm', '-i', '-v',
                            '/mnt/probe:/work', 'python:3.13-alpine', 'python', '-u', '/work/probe.py'],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err, text=True)
                        assert line(guest) == 'READY', 'guest did not cache the fixtures'
                        def host(action):
                            (share/'command').write_text(action)
                            def acknowledged():
                                answer = (share/'ack').read_text() if (share/'ack').exists() else ''
                                if answer.startswith('ERROR'):
                                    raise RuntimeError(answer)
                                return answer == action
                            wait_for(acknowledged, 30)
                        def check(action):
                            guest.stdin.write(action+'\n')
                            guest.stdin.flush()
                            answer = line(guest)
                            print(f'{mode}: {answer}', flush=True)
                            assert answer == 'PASS '+action, answer
                        host('mutate')
                        check('old')
                        host(mode)
                        check('new')
                        guest.stdin.close()
                        assert guest.wait(timeout=15) == 0
                        (share/'command').write_text('stop')
                finally:
                    if guest is not None and guest.poll() is None:
                        guest.kill()
                        guest.wait(timeout=10)
                    if vm.poll() is None:
                        vm.kill()
                    vm.wait(timeout=10)
    print('filesystem cache-reset probe passed', flush=True)


if __name__ == '__main__':
    main()
