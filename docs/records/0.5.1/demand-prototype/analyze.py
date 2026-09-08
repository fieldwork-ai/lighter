from pathlib import Path
import argparse,json,statistics as st,math
p=argparse.ArgumentParser();p.add_argument('record',type=Path);p.add_argument('--out',type=Path,required=True);a=p.parse_args()
env=json.loads((a.record/'environment.json').read_text());assert (a.record/'exit-code').read_text().strip()=='0';arms=[];images=[];profiles=[]
for i,mode in enumerate(env['order'],1):
 d=a.record/f'{i}-{mode}';assert (d/'exit-code').read_text().strip()=='0';r=json.loads((d/'results.json').read_text());assert len(r)==3
 e=json.loads((d/'environment.json').read_text());assert e['mode']==mode and e['saved_container'] and not e['timing'];profiles.append([e[k] for k in ['cli_sha256','payload','cpus','memory_mib','disk_gib']]);images.append(json.loads((d/'16384-image.json').read_text())['Id'])
 guard=[json.loads(s) for s in (a.record/f'{i}-{mode}-guard.jsonl').read_text().splitlines()];assert all(not row.get('unexpected') for row in guard)
 quiet=[row for row in guard if row.get('phase')=='quiet'];assert len(quiet)>=6 and all(row['machine_cpu_percent']<=5 for row in quiet[-6:])
 metrics={}
 for k in ['docker_ms','first_container_ms']:
  v=[row[k] for row in r];assert all(math.isfinite(x) and x>0 for x in v);metrics[k]=dict(median_ms=st.median(v),cv_percent=100*st.stdev(v)/st.mean(v),min_ms=min(v),max_ms=max(v))
 arms.append(dict(arm=i,mode=mode,metrics=metrics))
assert len(set(images))==1 and all(x==profiles[0] for x in profiles)
modes={}
for mode in dict.fromkeys(env['order']):
 modes[mode]={k:st.geometric_mean(row['metrics'][k]['median_ms'] for row in arms if row['mode']==mode) for k in ['docker_ms','first_container_ms']}
changes={mode:{k:100*(value/modes['background'][k]-1) for k,value in metrics.items()} for mode,metrics in modes.items()}
a.out.mkdir(exist_ok=True)
(a.out/'analysis.json').write_text(json.dumps(dict(arms=arms,geometric_means_of_arm_medians=modes,percent_change_from_background=changes,profile=profiles[0],image=images[0]),indent=2)+'\n')
lines=['# Controlled M1 demand-backed RAM prototype','', '8 vCPUs, 16 GiB guest RAM, 128 GiB sparse disk, saved stopped Alpine container. Three cold starts per arm, identical executable and guest payload, quiet-host and competing-VM guard. No repetition discarded. These are prototype measurements, not the immutable signed release archive.','', '| Arm | Mode | Docker median | CV | First container median | CV |','|---|---|---:|---:|---:|---:|']
for row in arms:
 d=row['metrics']['docker_ms'];f=row['metrics']['first_container_ms'];lines.append(f"| {row['arm']} | {row['mode']} | {d['median_ms']:.1f} ms | {d['cv_percent']:.2f}% | {f['median_ms']:.1f} ms | {f['cv_percent']:.2f}% |")
lines+=['','Geometric means of arm medians, with changes relative to background preparation:','']
for mode,metrics in modes.items():lines.append(f"- {mode}: Docker {metrics['docker_ms']:.1f} ms ({changes[mode]['docker_ms']:+.2f}%), first container {metrics['first_container_ms']:.1f} ms ({changes[mode]['first_container_ms']:+.2f}%).")
(a.out/'REPORT.md').write_text('\n'.join(lines)+'\n');print(json.dumps(dict(means=modes,changes=changes),indent=2))
