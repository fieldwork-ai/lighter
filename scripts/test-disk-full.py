#!/usr/bin/env python3
"""Real ENOSPC gate on a capped APFS image, never on the host's main filesystem.

Uses a private LIGHTER_HOME. Raw output stays in ignored .logs. Requires macOS,
a signed candidate CLI, and a guest bundle. --hold controls sustained blockage.
"""
import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import socket
import subprocess as sp
import tempfile
import time


def run(argv, **kwargs):
    return sp.run(argv, check=True, capture_output=True, text=True, **kwargs).stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/release/lighter'))
    parser.add_argument('--guest', type=Path, required=True)
    parser.add_argument('--filesystem', choices=['ext4', 'btrfs'], default='ext4')
    parser.add_argument('--hold', type=int, default=600)
    parser.add_argument('--poll-us', default='200')
    parser.add_argument('--cycles', type=int, default=1)
    parser.add_argument('--both-disks', action='store_true')
    parser.add_argument('--database', action='store_true', help='exercise a PostgreSQL transaction during exhaustion')
    args = parser.parse_args()
    if args.hold < 0 or args.cycles < 1:
        parser.error('--hold must be nonnegative and --cycles must be positive')
    binary = args.binary.resolve()
    print('Candidate sha256:', hashlib.sha256(binary.read_bytes()).hexdigest(), flush=True)
    fixture = Path(tempfile.mkdtemp(prefix='lighter-enospc-'))
    home = fixture / 'vm'
    home.mkdir()
    output = Path('.logs') / f'enospc-{args.filesystem}-{args.poll_us}-{time.time_ns()}'
    output.mkdir(parents=True)
    # A released guest directory contains its own executable bundle. Expose
    # only guest assets so bundle::ensure launches the candidate we requested.
    guest_dir = fixture / 'guest'
    guest_dir.mkdir()
    for name in ('Image', 'rootfs.ext4', 'kernel.version'):
        source = args.guest.resolve() / name
        if source.exists():
            (guest_dir / name).symlink_to(source)
    env = dict(os.environ, LIGHTER_HOME=str(home), LIGHTER_GUEST_DIR=str(guest_dir),
               LIGHTER_CMDLINE_EXTRA=f'lighter.data_fs={args.filesystem} lighter.fstrim_period=0',
               LIGHTER_HOST_POLL_US=args.poll_us)
    device = None
    database_writer = None
    filler = None
    started = False

    def guest(command, timeout=20):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(timeout)
            connection.connect(str(home / 'control.sock'))
            connection.sendall(('sh ' + command + '\n').encode())
            data = b''
            while b'--end--\n' not in data:
                chunk = connection.recv(65536)
                if not chunk:
                    raise RuntimeError('guest control disconnected')
                data += chunk
        result = data.decode(errors='replace')
        if not result.endswith('exit=0\n--end--\n'):
            raise RuntimeError(result)
        return result

    def storage():
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(2)
            connection.connect(str(home / 'status.sock'))
            data = b''
            while chunk := connection.recv(65536):
                data += chunk
        return json.loads(data)['waiting']

    try:
        # A fixed logical capacity bounds the damage even if the filler is wrong.
        image = fixture / 'host.sparseimage'
        run(['hdiutil', 'create', '-size', '2g', '-type', 'SPARSE', '-fs', 'APFS',
             '-volname', 'lighter-enospc', str(image)], timeout=60)
        mounted = plistlib.loads(sp.run(['hdiutil', 'attach', '-nobrowse', '-plist', str(image)],
                                      check=True, capture_output=True, timeout=30).stdout)
        entities = mounted['system-entities']
        device = next(e['dev-entry'] for e in entities if 'mount-point' in e)
        mount = Path(next(e['mount-point'] for e in entities if 'mount-point' in e))
        assert mount.is_mount() and mount.stat().st_dev != home.stat().st_dev
        (home / 'data.img').symlink_to(mount / 'data.img')
        (home / 'config.json').write_text(json.dumps(dict(cpus=2, memory_mib=2048,
                                                         disk_gib=8, shares=[], publish='localhost')))
        print(f'Fixture {fixture}; evidence {output}', flush=True)
        started = True
        print(run([str(binary), 'start'], env=env, timeout=90), flush=True)
        if args.both_disks:
            run([str(binary), 'stop'], env=env, timeout=40)
            root = home / 'rootfs.ext4'
            # Preserve sparseness across filesystems, avoiding a fully allocated
            # root image whose in-place writes might not need any new blocks.
            with root.open('rb') as source, (mount / 'rootfs.ext4').open('wb') as dest:
                while chunk := source.read(1024 * 1024):
                    if any(chunk):
                        dest.write(chunk)
                    else:
                        dest.seek(len(chunk), 1)
                dest.truncate(source.tell())
            root.unlink()
            root.symlink_to(mount / 'rootfs.ext4')
            print(run([str(binary), 'start'], env=env, timeout=90), flush=True)
        docker = ['docker', '--host', 'unix://' + str(home / 'docker.sock')]
        if args.database:
            run(docker + ['run', '-d', '--name', 'enospc-postgres', '--restart', 'unless-stopped',
                          '-e', 'POSTGRES_HOST_AUTH_METHOD=trust', 'postgres:18-alpine'], timeout=180)
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                if sp.run(docker + ['exec', 'enospc-postgres', 'pg_isready', '-U', 'postgres'],
                          capture_output=True).returncode == 0:
                    break
                time.sleep(0.5)
            else:
                raise RuntimeError('PostgreSQL did not become ready')
        guest('mount | grep /mnt/data; dd if=/dev/urandom of=/run/expected bs=1M count=8; sha256sum /run/expected > /run/expected.sha')
        for cycle in range(args.cycles):
            guest('rm -f /run/enospc-go /run/enospc-result /mnt/data/enospc-probe')
            guest('(while [ ! -e /run/enospc-go ]; do sleep 0.1; done; cp /run/expected /mnt/data/enospc-probe && sync && cmp /run/expected /mnt/data/enospc-probe; echo $? > /run/enospc-result) > /run/enospc-writer.log 2>&1 &')
            if args.both_disks:
                guest('rm -f /run/enospc-root-result /enospc-root-probe; (while [ ! -e /run/enospc-go ]; do sleep 0.1; done; for i in $(seq 1 32); do cat /run/expected || exit; done > /enospc-root-probe && sync; echo $? > /run/enospc-root-result) > /run/enospc-root.log 2>&1 &')
            # Allocate host blocks until this *mounted image* returns real ENOSPC.
            filler = mount / 'filler'
            block = os.urandom(1024 * 1024)
            with filler.open('wb', buffering=0) as f:
                try:
                    for _ in range(2300):
                        f.write(block)
                    raise RuntimeError('fixture exceeded its declared capacity')
                except OSError as error:
                    if error.errno != errno.ENOSPC:
                        raise
            guest('touch /run/enospc-go')
            if args.database:
                query = f"CREATE TABLE enospc_probe_{cycle} AS SELECT n, repeat(md5(n::text), 200) AS payload FROM generate_series(1,10000) AS n; CHECKPOINT;"
                database_writer = sp.Popen(docker + ['exec', 'enospc-postgres', 'psql', '-v', 'ON_ERROR_STOP=1', '-U', 'postgres', '-c', query], stdout=sp.PIPE, stderr=sp.STDOUT, text=True)

            deadline = time.monotonic() + 45
            waiting = []
            while time.monotonic() < deadline:
                waiting = storage()
                if len(waiting) >= (2 if args.both_disks else 1):
                    break
                time.sleep(0.2)
            assert len(waiting) >= (2 if args.both_disks else 1), f'required deferred requests not observed: {waiting}'
            print('BLOCKED', json.dumps(waiting), flush=True)
            began = time.monotonic()
            observations = []
            sampled = False
            while time.monotonic() - began < args.hold:
                state = storage()
                assert state, 'disk resumed while fixture remains full'
                if args.both_disks:
                    with socket.socket(socket.AF_UNIX) as connection:
                        connection.settimeout(3)
                        connection.connect(str(home / 'control.sock'))
                        connection.sendall(b'ping\n')
                        assert connection.recv(64) == b'pong\n'
                else:
                    health = guest('echo agent-alive; dmesg | grep -E "Aborting journal|Remounting filesystem read-only|EXT4-fs error|BTRFS.*error" || true')
                    assert 'Aborting journal' not in health and 'filesystem read-only' not in health and 'EXT4-fs error' not in health and 'BTRFS error' not in health, health
                host_cpu = float(run(['ps', '-o', '%cpu=', '-p', (home / 'lighter.pid').read_text().strip()]).strip())
                if host_cpu > 50 and time.monotonic() - began > 30 and not sampled and not args.both_disks:
                    sampled = True
                    diagnostic = guest('for pid in $(pidof dockerd containerd); do for p in /proc/$pid/task/*; do echo TASK $p $(cat $p/comm) $(cat $p/wchan); cat $p/stack; done; done')
                    (output / f'blocked-stacks-{cycle}.txt').write_text(diagnostic)
                observations.append(dict(elapsed=time.monotonic() - began, storage=state, host_cpu_percent=host_cpu))
                if len(observations) == 1 or len(observations) % 10 == 0:
                    print(f'Waiting {int(time.monotonic() - began)}s; retries={state[0]["retries"]}; host CPU={host_cpu}%; guest agent responsive', flush=True)
                time.sleep(min(3, max(0, args.hold - (time.monotonic() - began))))
            print(run([str(binary), 'status'], env=env, timeout=5), flush=True)
            filler.unlink()
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                if not storage() and 'result=0' in guest('echo result=$(cat /run/enospc-result 2>/dev/null)'):
                    break
                time.sleep(0.5)
            else:
                raise RuntimeError('writer did not recover')
            if args.both_disks:
                assert 'result=0' in guest('echo result=$(cat /run/enospc-root-result)')
                expected_root = guest('(for i in $(seq 1 32); do cat /run/expected; done) | sha256sum').split()[0]
                assert guest('sha256sum /enospc-root-probe').split()[0] == expected_root
            errors = guest('dmesg | grep -E "Aborting journal|Remounting filesystem read-only|EXT4-fs error|BTRFS.*(error|abort)" || true')
            assert errors.strip() == 'exit=0\n--end--', errors
            check = guest('cmp /run/expected /mnt/data/enospc-probe && sha256sum /mnt/data/enospc-probe; mount | grep /mnt/data; cat /run/enospc-writer.log')
            assert 'emergency_ro' not in check, check
            print('RECOVERED', check, flush=True)
            if args.database:
                stdout, _ = database_writer.communicate(timeout=60)
                assert database_writer.returncode == 0, stdout
                database_writer = None
                assert run(docker + ['exec', 'enospc-postgres', 'psql', '-At', '-U', 'postgres', '-c',
                                    f"SELECT count(*) FROM enospc_probe_{cycle} WHERE payload = repeat(md5(n::text),200);"]).strip() == '10000'
            expected_hash = guest('cat /run/expected.sha').split()[0]
        run([str(binary), 'stop'], env=env, timeout=40)
        print(run([str(binary), 'start'], env=env, timeout=90), flush=True)
        actual_hash = guest('sha256sum /mnt/data/enospc-probe').split()[0]
        assert actual_hash == expected_hash, 'content changed across clean reboot'
        if args.both_disks:
            assert guest('sha256sum /enospc-root-probe').split()[0] == expected_root
        if args.database:
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                if sp.run(docker + ['exec', 'enospc-postgres', 'pg_isready', '-U', 'postgres'], capture_output=True).returncode == 0:
                    break
                time.sleep(0.5)
            for cycle in range(args.cycles):
                assert run(docker + ['exec', 'enospc-postgres', 'psql', '-At', '-U', 'postgres', '-c',
                                    f"SELECT count(*) FROM enospc_probe_{cycle} WHERE payload = repeat(md5(n::text),200);"]).strip() == '10000'
        (output / 'observations.json').write_text(json.dumps(observations, indent=2))
        print(f'PASS {args.filesystem}: {args.cycles} cycle(s), {args.hold}s blockage each; database={args.database}, both_disks={args.both_disks}; verified content after reboot', flush=True)
    finally:
        # Give pending writes space before asking the disposable VM to stop.
        if filler is not None and filler.exists():
            filler.unlink()
        if database_writer is not None:
            database_writer.terminate()
            database_writer.communicate(timeout=10)
        if started:
            try:
                run([str(binary), 'stop'], env=env, timeout=40)
            except (sp.SubprocessError, OSError) as error:
                print(f'Cleanup stop failed: {error}; preserve fixture {fixture}', flush=True)
                device = None  # Never detach a disk still in use.
            if (home / 'machine.log').exists():
                shutil.copyfile(home / 'machine.log', output / 'machine.log')
        if device:
            run(['hdiutil', 'detach', device], timeout=30)
            shutil.rmtree(fixture)


if __name__ == '__main__':
    main()
