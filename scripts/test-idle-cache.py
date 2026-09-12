#!/usr/bin/env python3
"""Hardware regression: nested live code stays cached; an empty VM still trims.

Owns a private, shareless VM. Never drops caches in the daily VM. The pinned
BuildKit daemon exercises shared executable pages and nested cgroups without
running builds or depending on a busy host's benchmark timings.
"""
import argparse
import json
import os
from pathlib import Path
import shlex
import socket
import subprocess
import time

IMAGE = 'moby/buildkit@sha256:ddd1ca44b21eda906e81ab14a3d467fa6c39cd73b9a39df1196210edcb8db59e'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin', type=Path, required=True)
    parser.add_argument('--guest-dir', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    home = out / 'vm'
    home.mkdir()
    (home / 'config.json').write_text(json.dumps(dict(
        cpus=4, memory_mib=4096, disk_gib=64, shares=[], publish='localhost')))
    env = {k: v for k, v in os.environ.items() if not k.startswith('LIGHTER_')}
    env.update(LIGHTER_HOME=str(home), LIGHTER_GUEST_DIR=str(args.guest_dir.resolve()))
    for key in ('DOCKER_HOST', 'DOCKER_CONTEXT', 'BUILDX_BUILDER'):
        env.pop(key, None)
    binary = str(args.bin.resolve())
    docker = ['docker', '--host', f'unix://{home}/docker.sock']
    builder = f'lighter-cache-test-{os.getpid()}'
    container = f'buildx_buildkit_{builder}0'
    created = False

    def run(command, timeout=30):
        return subprocess.check_output(command, text=True, stderr=subprocess.STDOUT, timeout=timeout, env=env)

    def guest(command):
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(15)
            sock.connect(str(home / 'control.sock'))
            sock.sendall(('sh ' + command + '\n').encode())
            data = b''
            while b'--end--\n' not in data:
                part = sock.recv(65536)
                if not part:
                    break
                data += part
        text = data.decode()
        if '\nexit=0\n--end--\n' not in text:
            raise RuntimeError(text)
        return text.split('\nexit=0\n')[0]

    def engine_file():
        return int(guest("awk '$1 == \"file\" {print $2}' /sys/fs/cgroup/engine/memory.stat"))

    try:
        with (out / 'start.log').open('w') as log:
            subprocess.run([binary, 'start'], env=env, stdout=log, stderr=subprocess.STDOUT,
                           timeout=75, check=True)
        assert not run(docker + ['ps', '-aq']).strip(), 'test VM must be empty'
        run(docker + ['buildx', 'create', '--name', builder, '--driver', 'docker-container',
                      '--driver-opt', f'image={IMAGE}', f'unix://{home}/docker.sock'])
        created = True
        (out / 'bootstrap.log').write_text(run(docker + ['buildx', 'inspect', builder, '--bootstrap'], 120))
        info = json.loads(run(docker + ['inspect', container]))[0]
        layers = info['GraphDriver']['Data']
        candidates = [layers['UpperDir']] + layers['LowerDir'].split(':')
        commands = [f'if test -f {shlex.quote(p + "/usr/bin/buildkitd")}; then echo {shlex.quote(p + "/usr/bin/buildkitd")}; fi' for p in candidates]
        found = guest('; '.join(commands)).splitlines()
        assert len(found) == 1, found
        executable = found[0]
        run(docker + ['stop', '--timeout', '10', container])
        guest('sync; echo 3 > /proc/sys/vm/drop_caches')
        # A separate shell moves itself, never the control agent, to the engine.
        warm = 'echo $$ > /sys/fs/cgroup/engine/cgroup.procs; cat "$1" > /dev/null'
        guest('sh -c ' + shlex.quote(warm) + ' sh ' + shlex.quote(executable))
        run(docker + ['start', container])
        info = json.loads(run(docker + ['inspect', container]))[0]
        path = guest(f"cat /proc/{info['State']['Pid']}/cgroup").strip().split('0::', 1)[1]
        parent = '/sys/fs/cgroup' + path.removesuffix('/init')
        pid = int(guest(f'for p in $(cat {parent}/init/cgroup.procs); do '
                        'if [ "$(cat /proc/$p/comm)" = buildkitd ]; then echo "$p"; fi; done'))
        population = guest('cat /sys/fs/cgroup/docker/cgroup.events')
        assert 'populated 1' in population
        # The original immediate-child process count misclassified this VM.
        direct = guest('for c in /sys/fs/cgroup/docker/*; do '
                       'test ! -d "$c" || cat "$c/cgroup.procs"; done')
        assert not direct.strip(), direct

        def resident_file():
            return int(guest(f"awk '$1 == \"RssFile:\" {{print $2}}' /proc/{pid}/status"))

        before = resident_file()
        assert before > 16 * 1024, f'executable was not warm: {before} KiB'
        time.sleep(15)
        after = resident_file()
        result = dict(live_file_kib_before=before, live_file_kib_after=after,
                      population=population, immediate_child_processes=direct)
        (out / 'live.json').write_text(json.dumps(result, indent=2) + '\n')
        assert after >= before * 0.8, f'live executable evicted: {before} -> {after} KiB'
        cached = engine_file()
        assert cached > 16 * 1024**2, f'no engine cache to reclaim: {cached}'
        run(docker + ['stop', '--timeout', '10', container])
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            empty = 'populated 0' in guest('cat /sys/fs/cgroup/docker/cgroup.events')
            remaining = engine_file()
            if empty and remaining < cached / 2:
                break
            time.sleep(1)
        else:
            raise AssertionError(f'empty hierarchy did not trim: {cached} -> {remaining}')
        result.update(empty_engine_file_before=cached, empty_engine_file_after=remaining)
        (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(result, indent=2))
    finally:
        if created:
            try:
                subprocess.run(docker + ['buildx', 'rm', '--force', builder], env=env,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
            except subprocess.TimeoutExpired:
                subprocess.run(docker + ['buildx', 'rm', '--force', '--keep-daemon', builder],
                               env=env, timeout=30, check=False)
        with (out / 'stop.log').open('w') as log:
            subprocess.run([binary, 'stop'], env=env, stdout=log, stderr=subprocess.STDOUT,
                           timeout=45, check=True)
        if (home / 'machine.log').exists():
            (home / 'machine.log').rename(out / 'machine.log')
        # Keep diagnostics, not multi-gigabyte disposable VM disks.
        import shutil
        shutil.rmtree(home)


if __name__ == '__main__':
    main()
