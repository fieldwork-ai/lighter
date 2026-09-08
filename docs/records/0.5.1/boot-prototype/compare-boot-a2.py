from pathlib import Path
import subprocess as sp,json,statistics,math
root=Path.cwd();out=root/'.logs/051/m5-comparison-a2';out.mkdir(exist_ok=False)
order=['eager','background','background','eager','background','eager','eager','background'];records=[]
try:
 for memory in [16384,12288]:
  for i,mode in enumerate(order,1):
   path=out/f'{memory}-{i}-{mode}'
   cmd=['python3',str(root/'.logs/051/record-boot.py'),'--cli',str(root/'.logs/051/lighter-background-a2'),'--guest',str(root/'.logs/051/guest-a2'),'--memory',str(memory),'--reps','3','--out',str(path)]
   if mode=='background':cmd+=['--background']
   with (out/f'{memory}-{i}-{mode}.log').open('w') as log:sp.run(cmd,stdout=log,stderr=sp.STDOUT,check=True)
   rows=json.loads((path/'results.json').read_text());record={'memory_mib':memory,'arm':i,'mode':mode,'docker_ms':statistics.median(r['docker_ms'] for r in rows),'first_container_ms':statistics.median(r['first_container_ms'] for r in rows)}
   records.append(record);(out/'arm-medians.json').write_text(json.dumps(records,indent=2)+'\n');print(json.dumps(record),flush=True)
 (out/'exit-code').write_text('0\n')
except BaseException as error:
 (out/'failure.json').write_text(json.dumps({'error':str(error)})+'\n');(out/'exit-code').write_text('1\n');raise
summary=[]
for memory in [16384,12288]:
 for label,arms in [('ABBA',range(1,5)),('BAAB',range(5,9))]:
  rows=[r for r in records if r['memory_mib']==memory and r['arm'] in arms]
  for metric in ['docker_ms','first_container_ms']:
   b=statistics.geometric_mean(r[metric] for r in rows if r['mode']=='background');a=statistics.geometric_mean(r[metric] for r in rows if r['mode']=='eager')
   summary.append({'memory_mib':memory,'sequence':label,'metric':metric,'eager_ms':a,'background_ms':b,'change_percent':100*(b/a-1)})
(out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n');print(json.dumps(summary,indent=2),flush=True)
