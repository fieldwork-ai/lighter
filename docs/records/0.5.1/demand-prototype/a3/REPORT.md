# Controlled M1 demand-backed RAM prototype

8 vCPUs, 16 GiB guest RAM, 128 GiB sparse disk, saved stopped Alpine container. Three cold starts per arm, identical executable and guest payload, quiet-host and competing-VM guard. No repetition discarded. These are prototype measurements, not the immutable signed release archive.

| Arm | Mode | Docker median | CV | First container median | CV |
|---|---|---:|---:|---:|---:|
| 1 | background | 1608.3 ms | 1.57% | 1788.9 ms | 1.34% |
| 2 | demand | 958.8 ms | 2.81% | 1143.3 ms | 2.26% |
| 3 | demand-all | 696.6 ms | 3.58% | 872.5 ms | 2.50% |
| 4 | demand-all | 684.5 ms | 5.16% | 864.6 ms | 3.70% |
| 5 | demand | 949.9 ms | 1.27% | 1130.0 ms | 0.78% |
| 6 | background | 1624.6 ms | 1.13% | 1800.6 ms | 0.95% |

Geometric means of arm medians, with changes relative to background preparation:

- background: Docker 1616.4 ms (+0.00%), first container 1794.8 ms (+0.00%).
- demand: Docker 954.3 ms (-40.96%), first container 1136.6 ms (-36.67%).
- demand-all: Docker 690.5 ms (-57.28%), first container 868.5 ms (-51.61%).
