import csv,datetime,json,pathlib,statistics
root=pathlib.Path(__file__).resolve().parent
def load(path):
 values={}
 for row in csv.DictReader(path.open()):values.setdefault(row['case'],[]).append(float(row['ms']))
 return {k:statistics.median(v) for k,v in values.items()}
def unit(case):
 if case.startswith('memory-'):return 'MiB'
 if case=='power-cpu-ms-per-s':return 'CPU ms/s'
 if 'wakeups' in case:return 'wakeups/s'
 if case=='power-energy-x10':return 'energy ×10'
 if case in ('net-http-latency','net-http-p99','net-dns'):return 'µs'
 if case=='net-connect-rate':return 'connections/s'
 if case.startswith('net-'):return 'Mbit/s'
 return 'ms'
lines=['# 0.4.2 release measurements','','The primary record is the first full M1 suite, selected before the run. Two further consecutive suites measure repeatability and exercise filesystem-daemon recovery. Two later M5 attempts were invalidated by competing VMs and excluded; [their diagnostic records](results/releases/0.4.2/m5-invalid/) explain why no new M5 performance record is selected.','','The comparison baseline is the complete 0.4.1 run after the user restarted fseventsd on 2026-09-07, source `5f48cf03` (the released 0.4.1 runtime). The new suites use `cc717e79`; runtime `7d2123d` is unchanged by the final formula checksum commit. Each stage starts a fresh VM with eight vCPUs, 4 GiB RAM and a 128 GiB sparse data disk. The boot case uses the same CLI defaults as the baseline. Each timing case has an untimed warm-up and three ordered repetitions; memory and power rows are single sampling windows.','','The baseline and new suites use the same benchmark cases and the same M1. Six consecutive quiet samples ten seconds apart precede the first new suite. There is no daemon reset between suites. These are cross-session numeric changes, not causal speedup estimates. Workload, cache, thermal and background variation remain possible. The three-run CV is descriptive and is not a universal regression threshold.','','Raw CSVs, source/artifact stamps, monitoring samples and the exact soak protocol are retained in [the release records](results/releases/0.4.2/).']
share_baseline=load(root/'baseline-0.4.1-share.csv')
share_runs=[load(root/f'042-cc717e7-m1-{n}-share.csv') for n in (1,2,3)]
changes={case:[(run[case]/share_baseline[case]-1)*100 for run in share_runs] for case in ('npm-install','pnpm-install','yarn-install')}
abba=[load(root/'abba'/f'042-m1-abba-{n}-{v}.csv') for n,v in ((1,'041'),(2,'042'),(3,'042'),(4,'041'))]
abba_changes={case:(statistics.geometric_mean([abba[1][case],abba[2][case]])/statistics.geometric_mean([abba[0][case],abba[3][case]])-1)*100 for case in ('npm-install','pnpm-install','yarn-install')}
lines += ['', 'A subsequent [alternating M1 comparison](RELEASE-0.4.2-ABBA.md) produced changes of ' + ', '.join(f'{case}: {change:+.2f}%' for case,change in abba_changes.items()) + '. The larger apparent slowdown below did not reproduce consistently; the original full-suite numbers remain intact.', '', '## Reading the comparison', '', 'Host-share installation times are numerically higher in the primary 0.4.2 run. Across all three new runs, changes versus the single fresh 0.4.1 baseline were ' + '; '.join(f'{case}: {min(values):+.1f}% to {max(values):+.1f}%' for case,values in changes.items()) + '. These shifts are retained rather than dismissed as noise. A single baseline session and three later sessions do not distinguish release effects from session effects; no causal speedup or regression estimate is claimed.', '', 'The earlier five-run storage-only variance baseline remains in [REPEATABILITY.md](REPEATABILITY.md). Full-suite variation below can be larger: it includes changing cache state across storage, memory, networking and translated workloads.']
for stage in ('share','guest','amd64'):
 old=load(root/f'baseline-0.4.1-{stage}.csv')
 runs=[load(root/f'042-cc717e7-m1-{n}-{stage}.csv') for n in (1,2,3)]
 lines +=['',f'## M1 — {stage}','','| Metric | Unit | 0.4.1 | 0.4.2 primary | Numeric change | Run 2 | Run 3 | Between-run CV |','|---|---|---:|---:|---:|---:|---:|---:|']
 for case,value in runs[0].items():
  samples=[r[case] for r in runs];prior=old.get(case)
  change=f'{(value/prior-1)*100:+.1f}%' if prior else '—'
  cv=statistics.stdev(samples)/statistics.mean(samples)*100 if statistics.mean(samples) else 0
  lines.append(f'| {case} | {unit(case)} | {prior:g} | {value:g} | {change} | {samples[1]:g} | {samples[2]:g} | {cv:.1f}% |')
published=load(root/'published-0.4.1-share.csv')
fresh=load(root/'baseline-0.4.1-share.csv')
primary=load(root/'042-cc717e7-m1-1-share.csv')
lines += ['', '## Memory comparison context', '', 'The fresh-daemon baseline remains the comparison above. The previously published 0.4.1 record is retained as additional context, because the single memory windows vary materially between otherwise equivalent release workloads.', '', '| Memory metric (MiB) | Published 0.4.1 | Fresh-daemon 0.4.1 | Primary 0.4.2 |', '|---|---:|---:|---:|']
for metric in ('memory-peak','memory-after-15s','memory-after-60s','memory-idle'):
 lines.append(f'| {metric} | {published[metric]:g} | {fresh[metric]:g} | {primary[metric]:g} |')
lines += ['', 'There are no filesystem or memory-policy source changes in 0.4.2. Its guest agent was rebuilt with the new version number. Differences here do not establish a causal speedup or memory regression; all samples, including outliers, remain in the raw records.']
rows=[json.loads(line) for line in (root/'daemon.jsonl').read_text().splitlines()]
errors=[r for r in rows if 'error' in r];samples=[r for r in rows if 'pid' in r]
post=[r for r in samples if r['phase']=='post-suite'];baseline=[r for r in samples if r['phase']=='baseline']
assert samples and baseline and post and len({r['pid'] for r in samples})==1
post_seconds=(datetime.datetime.fromisoformat(post[-1]['utc'])-datetime.datetime.fromisoformat(post[0]['utc'])).total_seconds()
assert post_seconds >= 590, post_seconds
assert not errors, errors
assert (root/'exit-code').read_text().strip() == '0'
peaks=[vm['footprint_bytes']/1024**2 for r in samples for vm in r.get('vms',[]) if vm['footprint_bytes'] is not None]
lines+=['','## Filesystem-daemon soak','','![Filesystem-daemon memory and CPU over three suites](results/releases/0.4.2/fseventsd-soak.svg)','',f"All samples observed fseventsd PID {samples[0]['pid']}; it was not restarted. Monitoring spans {samples[0]['utc']} through {samples[-1]['utc']}. There were {len(samples)} valid samples and {len(errors)} monitor errors.",'',f"Daemon physical footprint: baseline median {statistics.median(r['footprint_mib'] for r in baseline):.2f} MiB; peak {max(r['footprint_mib'] for r in samples):.2f} MiB; final {samples[-1]['footprint_mib']:.2f} MiB. Transient CPU peak {max(r['cpu_percent'] for r in samples):.1f}%. During the ten-minute post-suite observation, median CPU was {statistics.median(r['cpu_percent'] for r in post):.1f}% and maximum {max(r['cpu_percent'] for r in post):.1f}%.",'',f"The largest sampled benchmark-VM physical footprint was {max(peaks):.1f} MiB. This is the macOS task ledger, including host overhead and compression; the configured 4 GiB limits guest RAM. Five-second monitoring can miss shorter peaks. The dedicated workload-memory cases above sample their own peaks more frequently.",'','A quiet result does not establish a fix for the original fseventsd incident. Its historical trigger remains unreproduced.']
(root.parents[2]/'RELEASE-0.4.2.md').write_text('\n'.join(lines)+'\n')
