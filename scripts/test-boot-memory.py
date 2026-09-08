#!/usr/bin/env python3
"""Exercise the guest's pre-restore memory gate against controlled sysfs states."""
import pathlib
import subprocess
import tempfile
import threading
import time
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / 'guest/rootfs/boot-memory'


class BootMemory(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='lighter-boot-memory-')
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        (self.root / 'block_size_bytes').write_text('08000000\n')
        for index in range(4):
            block = self.root / f'memory{index}'
            block.mkdir()
            (block / 'state').write_text('online\n' if index < 2 else 'offline\n')

    def gate(self, target):
        return subprocess.run(
            ['/bin/sh', str(SCRIPT), str(target), str(self.root)],
            capture_output=True, text=True, timeout=30,
        )

    def test_waits_for_online_memory_not_just_present_blocks(self):
        def online():
            time.sleep(0.2)
            for index in range(2, 4):
                (self.root / f'memory{index}/state').write_text('online\n')
        worker = threading.Thread(target=online)
        worker.start()
        start = time.monotonic()
        result = self.gate(512)
        worker.join()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertGreaterEqual(time.monotonic() - start, 0.2)
        self.assertIn('ready online_mib=512', result.stdout)

    def test_never_restores_with_incomplete_memory_after_timeout(self):
        result = self.gate(512)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('timeout online_mib=256 target_mib=512', result.stderr)

    def test_rejects_invalid_configuration_and_missing_geometry(self):
        self.assertNotEqual(self.gate('invalid').returncode, 0)
        (self.root / 'block_size_bytes').unlink()
        self.assertNotEqual(self.gate(256).returncode, 0)


if __name__ == '__main__':
    unittest.main()
