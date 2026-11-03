# lighter 0.13.9 on the M1 (2026-11-03)

The 8 GB M1 on macOS 26.6.2, OrbStack quit and the host quiet for a minute before each stage. 8 vCPUs and 4 GiB, fixed resources, three repetitions a case (`benchmarks/run.sh --target lighter --reps 3`, the share stage then `--where guest`).

- **0.13.9**: source 0d791ab (26293bd since: packaging and provisioning scripts only), kernel ab12b9d3, rootfs 5315b035 (`lighter.csv`, `lighter-guest.csv`).

The last full record on this machine is 0.8.0's (`../lighter.csv`), two months and many releases back, so a difference from it need not be this release's. To tell, the cases furthest from it were run again with 0.13.8's released guest (kernel 66b70047, rootfs 8cd0e537) and this one's, on the same binary, interleaved: 0.13.8, 0.13.9, 0.13.8, 0.13.9, three repetitions each (`ab-<round>-<guest>.csv`).

| median of 6 | 0.13.8's guest | 0.13.9's guest |
|---|---|---|
| copy the tree on the share | 4565 ms | 4452 ms |
| UDP, container to Mac | 4220 Mbit/s | 5110 Mbit/s |
| boot to dockerd | 802 ms | 816 ms |
| boot to the first container | 988 ms | 1013 ms |
| memory, idle (median of 2) | 377 MiB | 395 MiB |
| memory, at peak and after an install | same | same |

UDP reads 3.3 or 5.1 Gbit/s run to run with either guest. Copying the tree inside the full stage (10.7 s) follows the installs before it, as it did in 0.8.0's and 0.11.3's records here (5.4 to 16.7 s within one version). The cost of the guest is what its kernel adds: BTF and tracing for BPF, about 18 MiB of idle memory and 15 to 25 ms of boot here.
