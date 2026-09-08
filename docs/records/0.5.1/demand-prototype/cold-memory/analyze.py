import argparse,json,statistics as st,math,gzip
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('record',type=Path);a=p.parse_args();r=a.record
assert (r/'exit-code').read_text().strip()=='0'
env=json.loads((r/'environment.json').read_text());assert env['order']==['background','demand-all','demand-all','background'];arms=[]
for i,mode in enumerate(env['order'],1):
 d=r/f'{i}-{mode}';assert (d/'exit-code').read_text().strip()=='0';rows=json.loads((d/'results.json').read_text());assert len(rows)==3 and [x['rep'] for x in rows]==[0,1,2]
 guard=r/f'{i}-{mode}-guard.jsonl'
 text=guard.read_text() if guard.exists() else gzip.decompress(guard.with_suffix('.jsonl.gz').read_bytes()).decode()
 g=[json.loads(x) for x in text.splitlines()];assert all(not x.get('unexpected') for x in g);quiet=[x for x in g if x.get('phase')=='quiet'];assert len(quiet)>=6 and all(x['machine_cpu_percent']<=5 for x in quiet[-6:])
 for x in rows:assert len(x['allocations'])==2 and all(v['mib']==2048 and v['verify_ms']>0 for v in x['allocations'])
 values={'cli_start_ms':[x['start_ms'] for x in rows],'first_allocation_ms':[x['allocations'][0]['allocation_ms'] for x in rows],'second_allocation_ms':[x['allocations'][1]['allocation_ms'] for x in rows]}
 for v in values.values():assert all(math.isfinite(x) and x>0 for x in v)
 arms.append(dict(arm=i,mode=mode,metrics={k:dict(median_ms=st.median(v),cv_percent=100*st.stdev(v)/st.mean(v),values_ms=v) for k,v in values.items()}))
means={mode:{k:st.geometric_mean(x['metrics'][k]['median_ms'] for x in arms if x['mode']==mode) for k in arms[0]['metrics']} for mode in ['background','demand-all']}
changes={k:dict(ms=means['demand-all'][k]-means['background'][k],percent=100*(means['demand-all'][k]/means['background'][k]-1)) for k in means['background']}
result=dict(profile=env['profile'],arms=arms,geometric_means_of_arm_medians=means,changes=changes)
(r/'analysis.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(dict(means=means,changes=changes),indent=2))
