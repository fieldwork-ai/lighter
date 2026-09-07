import json, subprocess, time
from pathlib import Path
rows=[]
for line in subprocess.check_output(['ps','-axo','pid=,pcpu=,comm='],text=True).splitlines():
 fields=line.strip().split(None,2)
 if len(fields)!=3:continue
 pid,cpu,command=int(fields[0]),float(fields[1]),fields[2]
 if Path(command).name in ('ps','top'):continue
 rows.append(dict(pid=pid,cpu_percent=cpu,command=command))
limit=5*int(subprocess.check_output(['sysctl','-n','hw.ncpu'],text=True))
busy=sum(p['cpu_percent'] for p in rows)>limit
with Path('.logs/042/m5-retry/quiet-preflight.jsonl').open('a') as f:
 f.write(json.dumps(dict(time=time.time(),policy='aggregate-v2',busy=busy,total_cpu_percent=sum(p['cpu_percent'] for p in rows),processes=sorted(rows,key=lambda p:p['cpu_percent'],reverse=True)[:10]))+'\n')
print(int(busy))
