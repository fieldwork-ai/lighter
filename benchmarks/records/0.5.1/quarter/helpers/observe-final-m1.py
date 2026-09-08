"""Keep a ten-minute daemon observation after the last full suite, no reset."""
import subprocess as sp,time
from pathlib import Path
out=Path('.logs/051/m1-post-full-observation');out.mkdir(exist_ok=False)
(out/'phase').write_text('post-full-suite\n')
p=sp.Popen(['python3','scripts/records/monitor-host.py',str(out)])
try:
 for _ in range(60):
  time.sleep(10)
  assert p.poll() is None,'observer exited'
 (out/'exit-code').write_text('0\n')
finally:
 p.terminate();p.wait(10)
print('PASS: ten-minute post-suite observation retained',flush=True)
