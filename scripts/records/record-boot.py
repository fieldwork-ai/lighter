#!/usr/bin/env python3
"""Measure cold CLI startup in a private home, retaining every attempt.

This runner does not stop other VMs or assert that the host is quiet. Use the
benchmark guard on a dedicated host for controlled release measurements.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess as sp
import tempfile
import time


def digest(path):
    with Path(path).open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--cli', type=Path, required=True)
    ap.add_argument('--guest', type=Path, required=True)
    ap.add_argument('--out', type=Path, required=True)
    ap.add_argument('--memory', type=int, nargs='+', required=True)
    ap.add_argument('--cpus', type=int, default=8)
    ap.add_argument('--reps', type=int, default=10)
    ap.add_argument('--mode', choices=['default', 'eager', 'background', 'demand', 'demand-all'], default='default')
    ap.add_argument('--image', default='alpine:3.21')
    ap.add_argument('--saved-container', action='store_true')
    ap.add_argument('--timing', action='store_true')
    a = ap.parse_args()
    if a.reps < 1 or a.cpus < 1 or any(m < 128 or m % 128 for m in a.memory):
        ap.error('positive repetitions/CPUs and memory in whole 128 MiB blocks required')
    out = a.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    cli, guest = a.cli.resolve(), a.guest.resolve()
    env = {k: v for k, v in os.environ.items()
           if not k.startswith('LIGHTER_') and k not in ('DOCKER_HOST', 'DOCKER_CONTEXT', 'DOCKER_CONFIG')}
    if a.mode != 'default':
        env['LIGHTER_DEMAND_RAM'] = '1' if a.mode in ('demand', 'demand-all') else '0'
        env['LIGHTER_DEMAND_BASE'] = '1' if a.mode == 'demand-all' else '0'
        env['LIGHTER_BACKGROUND_RAM'] = '0' if a.mode == 'eager' else '1'
    if a.timing:
        env['LIGHTER_BOOT_TIMING'] = '1'
    env['LIGHTER_GUEST_DIR'] = str(guest)
    results = []

    def run(args, **kwargs):
        return sp.run(args, env=env, check=True, timeout=120, **kwargs)

    try:
        source = sp.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
        patch = sp.check_output(['git', 'diff', 'HEAD'])
        (out / 'source.patch').write_bytes(patch)
        metadata = dict(
            source=source, tracked_source_dirty=bool(patch), cli=str(cli),
            cli_sha256=digest(cli), guest=str(guest),
            payload={n: digest(guest / n) for n in ['Image', 'rootfs.ext4']},
            platform=sp.check_output(['sw_vers'], text=True),
            cpus=a.cpus, memory_mib=a.memory, disk_gib=128, reps=a.reps,
            mode=a.mode, timing=a.timing, saved_container=a.saved_container,
            quiet_host_enforced=False,
            sampling='docker version polled every 50 ms, plus command duration; first Alpine true with image present',
        )
        (out / 'environment.json').write_text(json.dumps(metadata, indent=2) + '\n')
        for memory in a.memory:
            home = Path(tempfile.mkdtemp(prefix='lighter-boot-record-', dir='/private/tmp'))
            env.update(LIGHTER_HOME=str(home), DOCKER_HOST=f'unix://{home}/docker.sock')
            if owner_file := os.environ.get('LIGHTER_BENCH_OWNER_FILE'):
                with Path(owner_file).open('a') as owners:
                    owners.write(json.dumps(str(home / 'lighter.app/Contents/MacOS/lighter')) + '\n')
            dk = ['docker', '--host', env['DOCKER_HOST']]
            (out / f'{memory}-home').write_text(str(home) + '\n')
            with (out / f'{memory}.log').open('x') as log:
                try:
                    run([cli, 'config', '--cpus', str(a.cpus), '--memory', str(memory), '--disk', '128'], stdout=log, stderr=sp.STDOUT)
                    run([cli, 'start', '--timeout', '120'], stdout=log, stderr=sp.STDOUT)
                    run(dk + ['pull', a.image], stdout=log, stderr=sp.STDOUT)
                    info = json.loads(run(dk + ['image', 'inspect', a.image], capture_output=True, text=True).stdout)[0]
                    image = info['Id']
                    (out / f'{memory}-image.json').write_text(json.dumps({n: info[n] for n in ['Id', 'RepoDigests']}, indent=2) + '\n')
                    run(dk + ['run', '--rm', image, 'true'], stdout=log, stderr=sp.STDOUT)
                    if a.saved_container:
                        run(dk + ['create', '--name', 'boot-gate-sentinel', image, 'true'], stdout=log, stderr=sp.STDOUT)
                    for rep in range(1, a.reps + 1):
                        run([cli, 'stop'], stdout=log, stderr=sp.STDOUT)
                        time.sleep(2)
                        processes = sp.check_output(['ps', '-axo', 'pid=,%cpu=,rss=,comm='], text=True)
                        (out / f'{memory}-{rep}-host.txt').write_text(processes)
                        offset = (home / 'machine.log').stat().st_size
                        wall_time = time.time()
                        t0 = time.monotonic_ns()
                        start = sp.Popen([cli, 'start', '--timeout', '120'], env=env, stdout=log, stderr=sp.STDOUT)
                        try:
                            while True:
                                probe = sp.run(dk + ['version'], env=env, stdout=sp.DEVNULL, stderr=sp.DEVNULL, timeout=5)
                                if probe.returncode == 0:
                                    break
                                if start.poll() is not None and start.returncode != 0:
                                    raise RuntimeError('start failed')
                                if time.monotonic_ns() - t0 > 120e9:
                                    raise TimeoutError('Docker readiness')
                                time.sleep(.05)
                            t1 = time.monotonic_ns()
                            run(dk + ['run', '--rm', image, 'true'], stdout=log, stderr=sp.STDOUT)
                            t2 = time.monotonic_ns()
                            if start.wait(timeout=120) != 0:
                                raise RuntimeError('start failed after Docker answered')
                        finally:
                            if start.poll() is None:
                                start.terminate()
                                start.wait(timeout=10)
                            with (home / 'machine.log').open('rb') as source_log:
                                source_log.seek(offset)
                                (out / f'{memory}-{rep}-machine.log').write_bytes(source_log.read())
                        row = dict(memory_mib=memory, rep=rep, docker_ms=(t1-t0)/1e6,
                                   first_container_ms=(t2-t0)/1e6, wall_time=wall_time)
                        results.append(row)
                        (out / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
                        print(json.dumps(row), flush=True)
                finally:
                    # Only this runner's private VM, using the generation-aware CLI.
                    run([cli, 'stop'], stdout=log, stderr=sp.STDOUT)
        summary = []
        for memory in a.memory:
            rows = [r for r in results if r['memory_mib'] == memory]
            for metric in ['docker_ms', 'first_container_ms']:
                values = [r[metric] for r in rows]
                summary.append(dict(memory_mib=memory, metric=metric,
                                    median_ms=statistics.median(values), min_ms=min(values), max_ms=max(values),
                                    cv_percent=100*statistics.stdev(values)/statistics.mean(values) if len(values)>1 else None))
        (out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
        (out / 'exit-code').write_text('0\n')
        print(json.dumps(summary, indent=2), flush=True)
    except BaseException as error:
        (out / 'failure.json').write_text(json.dumps(dict(type=type(error).__name__, error=str(error))) + '\n')
        (out / 'exit-code').write_text('1\n')
        raise


if __name__ == '__main__':
    main()
