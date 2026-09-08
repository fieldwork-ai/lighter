import datetime,json,pathlib,subprocess,time,signal
root=pathlib.Path(__file__).resolve().parent
home=pathlib.Path((root/'home-path').read_text());stop=False
def end(*args):
 global stop
 stop=True
signal.signal(signal.SIGTERM,end)
with (root/'resources.jsonl').open('a') as f:
 while not stop:
  row={'utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'phase':(root/'phase').read_text().strip()}
  try:
   pid=(home/'lighter.pid').read_text().strip()
   row['vm_pid']=int(pid)
   row['vm_ps']=subprocess.check_output(['ps','-p',pid,'-o','%cpu=,rss='],text=True).strip()
   p=subprocess.run(['/Users/nick/git/lighter/.logs/042/task-footprint',pid],capture_output=True,text=True,timeout=5)
   row['vm_footprint_bytes']=int(p.stdout) if p.returncode==0 else None
   pid=subprocess.check_output(['pgrep','-x','fseventsd'],text=True).strip()
   row['fseventsd_pid']=int(pid)
   row['fseventsd_ps']=subprocess.check_output(['ps','-p',pid,'-o','%cpu=,rss='],text=True).strip()
  except Exception as e:row['error']=str(e)
  f.write(json.dumps(row)+'\n');f.flush()
  for _ in range(25):
   if stop:break
   time.sleep(.2)
