"""Report the M1 follow-up without discarding the earlier full-suite comparison."""
import csv,json,statistics
from pathlib import Path
root=Path(__file__).resolve().parent
runs=[]
for index,version in ((1,'041'),(2,'042'),(3,'042'),(4,'041')):
    cases={}
    for row in csv.DictReader((root/f'042-m1-abba-{index}-{version}.csv').open()):
        cases.setdefault(row['case'],[]).append(float(row['ms']))
    assert all(len(v)==3 for v in cases.values())
    runs.append({k:statistics.median(v) for k,v in cases.items()})
assert (root/'exit-code').read_text().strip()=='0'
lines=['# 0.4.2 M1 alternating install comparison','','The three full M1 suites showed higher host-share install times against a single earlier 0.4.1 baseline. This follow-up checks those cases in alternating order on the same M1: **0.4.1 → 0.4.2 → 0.4.2 → 0.4.1**. All four arms completed.','','Each arm starts a fresh VM with eight vCPUs, 4 GiB guest RAM and a 128 GiB sparse data disk. It uses the same host-share fixture, an untimed warm-up, and three ordered repetitions for npm, pnpm and yarn. Both benchmark binaries were built and signed before the initial quiet preflight. There is a twenty-second gap after each arm and no daemon reset.','','The 0.4.1 benchmark was rebuilt from source `5f48cf03` and uses the guest payload extracted from the published 0.4.1 archive. The 0.4.2 benchmark was retained from the qualified runtime at source `d0fbd114` and uses its matching guest payload. The harness is the current benchmark runner with explicit prebuilt binary, guest and source-stamp selection; no compilation occurs during the arms. [Raw repetitions, artifact hashes and the exact protocol](results/releases/0.4.2/abba/) are retained.','','## Results','','Each cell is the median of three timings, in milliseconds. The relative change compares the geometric mean of the two arm medians for each version; this preserves the multiplicative scale of timing ratios.','','| Workload | 0.4.1 A1 | 0.4.2 B1 | 0.4.2 B2 | 0.4.1 A2 | Relative change |','|---|---:|---:|---:|---:|---:|']
for case in runs[0]:
    old=statistics.geometric_mean([runs[0][case],runs[3][case]])
    new=statistics.geometric_mean([runs[1][case],runs[2][case]])
    lines.append(f'| {case} | '+ ' | '.join(f'{run[case]:g}' for run in runs)+f' | {(new/old-1)*100:+.2f}% |')
rows=[json.loads(s) for s in (root/'daemon.jsonl').read_text().splitlines()]
assert not [r for r in rows if 'error' in r]
assert len({r['pid'] for r in rows})==1
lines +=['','The larger apparent slowdown in the earlier cross-session comparison did not reproduce consistently. In particular, the two 0.4.2 yarn medians moved in opposite directions relative to the surrounding 0.4.1 controls. These small aggregate differences do not establish general speedups or a regression rate: there are only two arms per version, and residual cache, thermal and background variation remain possible.','','The original [full-suite comparison](RELEASE-0.4.2.md), including its higher host-share timings, remains intact. This follow-up supplements it rather than replacing the baseline after seeing the outcome.','',f"The same fseventsd PID {rows[0]['pid']} was observed throughout the follow-up. Its sampled physical footprint peaked at {max(r['footprint_mib'] for r in rows):.2f} MiB. No historical daemon failure was reproduced."]
(root.parents[3]/'RELEASE-0.4.2-ABBA.md').write_text('\n'.join(lines)+'\n')
