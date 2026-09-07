import os,signal,subprocess,sys,time
from pathlib import Path
cap=float(sys.argv[1]); command=sys.argv[2:]
pidfile=Path.home()/'.lighter/lighter.pid'
def daily():
    try:
        pid=int(pidfile.read_text().strip())
        os.kill(pid,0)
        name=subprocess.check_output(['ps','-p',str(pid),'-o','comm='],text=True).strip()
        return pid if name.endswith('/lighter') else None
    except (FileNotFoundError,ValueError,ProcessLookupError,subprocess.CalledProcessError):
        return None
assert daily() is None, 'Daily VM is already running'
child=subprocess.Popen(command,start_new_session=True)
start=time.monotonic()
code=None
try:
    while child.poll() is None:
        pid=daily()
        if pid:
            print(f'ABORT: daily VM {pid} started during this stage',flush=True)
            code=125
            break
        if time.monotonic()-start>cap:
            print(f'CAPPED after {cap:g}s',flush=True)
            code=124
            break
        time.sleep(.5)
finally:
    if child.poll() is None:
        os.killpg(child.pid,signal.SIGTERM)
        try:child.wait(timeout=8)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid,signal.SIGKILL);child.wait()
sys.exit(code if code is not None else child.returncode)
