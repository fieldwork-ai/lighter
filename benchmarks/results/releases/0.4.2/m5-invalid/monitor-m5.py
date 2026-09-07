import subprocess as sp,sys,time,pathlib,re,json,signal,datetime
root=pathlib.Path(sys.argv[1]);stop=False

def stopping(*_):
 global stop
 stop=True
signal.signal(signal.SIGTERM,stopping);signal.signal(signal.SIGINT,stopping)
with (root/'daemon.jsonl').open('w') as rows,(root/'daemon.top').open('w') as raw:
 while not stop:
  start=time.monotonic()
  try:
   pid=int(sp.check_output(['pgrep','-x','fseventsd'],text=True).strip())
   text=sp.check_output(['top','-l','2','-s','1','-pid',str(pid),'-stats','pid,command,cpu,mem,cmprs'],text=True)
   raw.write(text);raw.flush()
   row=[s.split() for s in text.splitlines() if re.match(r'^'+str(pid)+r'\s',s)][-1]
   def mib(s):
    m=re.match(r'([0-9.]+)([BKMG])',s);return float(m[1])*dict(B=1,K=1024,M=1024**2,G=1024**3)[m[2]]/1024**2
   phase=(root/'phase').read_text().strip()
   entry=dict(utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),pid=pid,phase=phase,cpu_percent=float(row[2]),footprint_mib=mib(row[3]),compressed_mib=mib(row[4]))
   vms=[];host=[]
   for proc in sp.check_output(['ps','-axo','pid=,pcpu=,comm='],text=True).splitlines():
    fields=proc.strip().split(None,2)
    if len(fields)==3:host.append(dict(pid=int(fields[0]),cpu_percent=float(fields[1]),command=fields[2]))
    if len(fields)==3 and fields[2].endswith('/lighter-bench'):
     vm_pid=int(fields[0])
     probe=sp.run(['.logs/042/task-footprint',str(vm_pid)],text=True,capture_output=True)
     vms.append(dict(pid=vm_pid,footprint_bytes=int(probe.stdout) if probe.returncode==0 else None))
   entry['vms']=vms
   entry['host_processes']=sorted(host,key=lambda p:p['cpu_percent'],reverse=True)[:10]
   rows.write(json.dumps(entry)+'\n');rows.flush()
  except Exception as e:
   rows.write(json.dumps(dict(error=str(e),time=time.time()))+'\n');rows.flush()
  while not stop and time.monotonic()-start<5:time.sleep(.2)
