# Controlled M1 demand-backed RAM prototype

8 vCPUs, 16 GiB guest RAM, 128 GiB sparse disk, saved stopped Alpine container. Three cold starts per arm, identical executable and guest payload, quiet-host and competing-VM guard. No repetition discarded. These are prototype measurements, not the immutable signed release archive.

| Arm | Mode | Docker median | CV | First container median | CV |
|---|---|---:|---:|---:|---:|
| 1 | background | 1621.8 ms | 3.44% | 1801.1 ms | 2.89% |
| 2 | demand | 949.8 ms | 1.94% | 1137.8 ms | 1.55% |
| 3 | demand | 955.0 ms | 1.83% | 1135.2 ms | 1.02% |
| 4 | background | 1606.1 ms | 1.67% | 1782.7 ms | 1.33% |
| 5 | demand | 910.9 ms | 3.84% | 1088.0 ms | 5.75% |
| 6 | background | 1611.3 ms | 1.49% | 1789.3 ms | 1.43% |
| 7 | background | 1593.7 ms | 2.21% | 1767.7 ms | 2.22% |
| 8 | demand | 936.5 ms | 1.27% | 1111.6 ms | 0.77% |

Geometric means of arm medians, with changes relative to background preparation:

- background: Docker 1608.2 ms (+0.00%), first container 1785.2 ms (+0.00%).
- demand: Docker 937.9 ms (-41.68%), first container 1118.0 ms (-37.38%).
