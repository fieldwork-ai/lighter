Lighter 0.5.0 adds qualified Kubernetes development through kind and verified, explicitly activated updates. It includes the work from the unpublished 0.4.2 candidate.

- Run kind v0.33.0 with pinned Kubernetes 1.35.8, 1.36.4 or 1.37.0 arm64 nodes. One-node and two-node IPv4 clusters pass on M1 and M5, including Service networking, local images, Helm, Mac shares, persistent volumes and VM restart recovery. A separate two-node 1.37.0 configuration passes with nftables. See the Kubernetes guide for tested scope and setup.
- Short Docker commands retain their output through graceful socket shutdown. Host socket reads now handle a reproduced macOS EOF race, and abort closes both directions reliably. Final stress checks pass 10,000 short execs, 10,000 mixed stdin commands and 2,050 concurrent stream checks on each Mac.
- Direct installations can check and download verified releases, then activate them with `lighter upgrade`. A running VM requires `--restart`; a stopped VM remains stopped. Optional background downloads default off. Activation verifies the complete CLI, VM, kernel and root filesystem together and supports recovery after failed activation.
- Installation ownership keeps Homebrew upgrades with Homebrew. Existing official direct installations migrate through the installer; configuration, images, containers and volumes stay in the machine home.
- Linux remains 6.18.49, rebuilt with the match required for kind's default iptables Service rules. Kubernetes versions inside existing clusters do not change when Lighter updates.

The 0.4.1 memory-accounting correction is retained. Mixed kind, Docker and image-build pressure passed at 8, 12 and 16 GiB, with sampled host footprints below those configurations in these tests and the footprint charge falling afterward. Guest RAM settings do not impose an absolute ceiling on host allocations.

Use the installer to migrate an existing direct installation, or upgrade through Homebrew when the tap update is available. Preserve application data before deleting or recreating kind clusters.

Three full benchmark suites per Mac, five identical-build storage runs, alternating 0.4.1/0.5.0 comparisons and fresh competitor runs are recorded in the [benchmark report](https://github.com/fieldwork-ai/lighter/blob/v0.5.0/benchmarks/RELEASE-0.5.0.md) and updated README. Failed or contaminated competitor cases are explicitly excluded. Results do not support a blanket speedup claim: M1 Yarn and M5 share copying remain possible costs, while several apparent regressions reversed with comparison order. The historical fseventsd incident was not reproduced; no fix is claimed.

The final signed archive passes fresh installation, 0.4.1 and private 0.4.2 migration, activation, failed-boot rollback and two-node kind persistence tests on both M1 and M5. The M1 Homebrew upgrade passes repeated postinstall, manifest verification and updater ownership checks. Apple notarization, stapling and Gatekeeper assessment pass. See [qualification and artifact evidence](https://github.com/fieldwork-ai/lighter/blob/v0.5.0/docs/release-0.5.0.md).

SHA256:

- `lighter-0.5.0-arm64.tar.gz`: `4da3fca84dbaf6d1ede81d682efbb52df64db438cfeef9679d3e60eb05e8df52`
- `lighter-0.5.0-arm64`: `594b09e923358787fed3c67788fa95a6d76f172b8132daae3e55d64982b9438a`
