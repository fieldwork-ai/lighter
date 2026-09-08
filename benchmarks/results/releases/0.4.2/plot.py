"""Render the recorded fseventsd soak; requires matplotlib."""
import datetime
import json
from pathlib import Path
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt

root = Path(__file__).resolve().parent
rows = [json.loads(line) for line in (root / 'daemon.jsonl').read_text().splitlines()]
rows = [row for row in rows if 'pid' in row]
start = datetime.datetime.fromisoformat(rows[0]['utc'])
times = [(datetime.datetime.fromisoformat(row['utc']) - start).total_seconds() / 60 for row in rows]
starts = {}
for t, row in zip(times, rows):
    starts.setdefault(row['phase'], t)
assert all(f'{suite}-share' in starts for suite in (1, 2, 3)) and 'post-suite' in starts
plt.rcParams.update({'font.size': 10, 'svg.hashsalt': 'lighter-0.4.2-soak'})
fig, axes = plt.subplots(2, 1, figsize=(11, 5.4), sharex=True, layout='constrained')
axes[0].plot(times, [r['footprint_mib'] for r in rows], color='#205a91', linewidth=1.3, label='Physical footprint')
axes[0].plot(times, [r['compressed_mib'] for r in rows], color='#a65920', linewidth=1, label='Compressed memory (included)')
axes[0].set_ylabel('Daemon memory (MiB)')
axes[0].set_ylim(bottom=0)
fig.legend(loc='outside lower center', ncol=2, frameon=False)
axes[1].plot(times, [r['cpu_percent'] for r in rows], color='#374451', linewidth=1)
axes[1].set_ylabel('CPU (% of one core)')
axes[1].set_ylim(bottom=0)
axes[1].set_xlabel('Minutes since monitoring began')
for axis in axes:
    axis.grid(axis='y', color='#e1e5e9', linewidth=.7)
    axis.spines[['top', 'right']].set_visible(False)
    axis.axvspan(starts['post-suite'], times[-1], color='#e9edf1', zorder=0)
    for suite in (1, 2, 3):
        axis.axvline(starts[f'{suite}-share'], color='#a5acb2', linestyle=':', linewidth=.8)
for suite in (1, 2, 3):
    left = starts[f'{suite}-share']
    right = starts[f'{suite+1}-share'] if suite < 3 else starts['post-suite']
    axes[0].text((left+right)/2, 1.01, f'Suite {suite}', transform=axes[0].get_xaxis_transform(), ha='center', va='bottom', color='#59636c')
axes[0].text((starts['post-suite']+times[-1])/2, 1.01, 'Post-suite', transform=axes[0].get_xaxis_transform(), ha='center', va='bottom', color='#59636c')
fig.suptitle('fseventsd during three consecutive full M1 benchmark suites', fontsize=13)
fig.savefig(root / 'fseventsd-soak.svg', metadata={'Date': None})
svg = root / 'fseventsd-soak.svg'
svg.write_text('\n'.join(line.rstrip() for line in svg.read_text().splitlines()) + '\n')
fig.savefig(root / 'fseventsd-soak.png', dpi=150)
