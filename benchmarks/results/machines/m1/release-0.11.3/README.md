# lighter 0.11.3 against 0.11.2 on the M1 (2026-10-04)

The 8 GB M1 Mac mini on macOS 26.6.2, one session, the home server stack stopped. Both at 8 vCPUs and 4 GiB, fixed resources, three repetitions a case, the same fixture and benchmark image (`benchmarks/run.sh`, through `benchmarks/compare.sh --targets lighter --stages "share guest"`).

- **0.11.3**: source 1c96d91, kernel 6225825f, rootfs e1f2a37a (`lighter-0.11.3*.csv`).
- **0.11.2**: source 155f4be (`lighter-bench` built from the tag), the released kernel 7ed412d7 and rootfs e88e5688 (`lighter-0.11.2*.csv`).

0.11.3's boot case met the 10 GiB disk floor in its pass (scratch from September runs was still on the disk), so it was run again alone after the 0.11.2 pass (`lighter-0.11.3-boot.csv`), as the 0.10.0 record did.

**Medians, where they differ by more than the repetitions do**

Every network case, boot, CPU, idle memory, npm and pnpm landed within a few percent of 0.11.2. Four share cases did not, and every one had repetitions further apart than the difference: yarn install 9.6 against 14.3 s (0.11.3's repetitions 15.4, 14.3, 9.6 s), copy the tree 4.4 against 5.2 s, the own disk's sequential write 0.72 against 0.98 s, and the first walk after the tree is made.

**The same cases interleaved**, 0.11.2, 0.11.3, 0.11.2, 0.11.3, three repetitions each (`ab-*.txt`):

| | 0.11.2 | 0.11.3 |
|---|---|---|
| yarn install on the share (median of 6) | 9.7 s | 9.7 s |
| copy the tree on the share (median of 6) | 11.7 s | 9.8 s |
| find over the tree, warm | 105–164 ms | 105–118 ms |
| ripgrep over the tree, warm | 125–170 ms | 125–138 ms |
| sequential write, own disk | 480–1868 ms | 499–1737 ms |

No regression. Copying the tree on this 8 GB machine ranged from 5.4 to 16.7 s within each version.
