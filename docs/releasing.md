# Releasing

A release is a signed, notarized tarball on GitHub, a Homebrew formula that points at it, a tag, and one pull request from `dev` to `main`. This is the order, with the traps found on 0.2.0.

## Before

- The full gate set green on the exact head you will ship: `scripts/gates/run-all.sh` (m1–m8). On a machine where a daily driver runs beside it, `LIGHTER_BENCH_ALLOW_NOISY=1` lets m5 measure; its numbers are not the record then, only the pass. The gates write their measurements to `benchmarks/results/gate-*.csv`, which git ignores; check `git status` under `benchmarks/results/` before the release commit all the same.
- Set `ulimit -S -n 10240` in the qualification shell before testing. Concurrent filesystem tests construct tables directly, while production raises the descriptor limit when starting the server; a fresh macOS shell at 256 descriptors cannot run this test workload. CI uses the same preparation.
- The CI commands themselves, verbatim, not the local habit: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`. 0.2.0's `dev` was red on five clippy warnings a local run had tolerated.
- The daily driver up on the same build for a working day (`make install` or the tarball), because the gates do not cover a Mac that sleeps and wakes, a VPN that changes the resolver, or a week of images.
- The README's numbers regenerated from the record CSVs, never typed: `python3 benchmarks/readme.py --write`. The record is a quiet machine; the M1's comes from `scripts/provision-bench-host.sh`'s host and a runner like `~/remote-record3.sh` there.

## The version

One number, in the workspace `Cargo.toml`: `version` and the four internal crates' `version` pins beside it. 0.2.0's bump missed the pins, the workspace stopped resolving mid-gate, and m7 and m8 failed for a reason that read as a guest problem. Bump all four, and the guest agent's own `guest/agent/Cargo.toml` (and its lockfile) beside them, then `cargo build`, `make guest` for the rootfs the agent lives in, and the gates again on the bumped head — the release commit is the one the gates saw.

## The tarball

```
scripts/package-release.sh 0.5.0
```

Builds release, signs `lighter` with the Developer ID Application identity and the hypervisor entitlement (`cargo build --release` alone strips it, which is what `make sign PROFILE=release` is for during development), submits to `notarytool`, and packs `dist/lighter-<version>-arm64.tar.gz` with the kernel, the rootfs and the entitlements. `--skip-notarize` is for checking the packaging, not for shipping. The packager restores the checkout binary's development entitlement immediately after building, requires an explicit Accepted notarization status, staples and validates the app ticket, and checks Gatekeeper before packing. Missing notarization credentials are an error unless `--skip-notarize` was explicitly requested. The production runtime and guest payload must match the qualified source: 0.2.0's first tarball was built one commit early and withdrawn. If final packaging follows documentation or formula-only commits, record both source commits, verify identical guest hashes and executable bytes after removing signatures from temporary copies, and repeat exact-archive smoke tests. Any runtime or guest change requires fresh qualification.

Before publishing:

1. Test the exact notarized archive, including a fresh install, `doctor`, an arm64 and amd64 container, and shutdown on both hardware hosts. Run `scripts/test-release-upgrade.sh` with the published 0.4.1 archive, candidate archive and signed bootstrap to verify real migration and data preservation.
2. Exercise `scripts/test-staged-upgrade.sh` with privately built, signed and notarized future fixtures: a valid next version and a nonbooting version whose manifest correctly authenticates the intentionally invalid kernel. This covers explicit restart, stopped-state preservation, tamper rejection, concurrent-updater rejection, interrupted-transaction recovery and rollback with container data. `LIGHTER_TEST_LOGIN=1` adds the real login service path and refuses to replace an existing service. Never publish these future fixtures.
3. Complete controlled full benchmarks on M1 and M5 and three consecutive full suites per host without resetting `fseventsd`, monitoring daemon memory/CPU and observing it for ten minutes afterward. Continuously reject competing VMs and retain failed or noisy attempts separately from performance evidence. Record raw results, source and artifact hashes, comparison uncertainty and any failures.
4. Update `packaging/lighter.rb` with the final archive URL and SHA256. Verify Homebrew ownership, repeated `post_install`, preserved signatures and refusal of direct upgrades from the Cellar. Prepare the Homebrew tap PR using the same formula.
5. Raise the qualified `dev` → `main` PR with test evidence and release notes. Keep the built archive and bootstrap immutable while the PR is reviewed. `main` moves only through this PR.
6. After the main PR merges, tag the qualified release source and publish `v<version>` with **both** `lighter-<version>-arm64.tar.gz` and the signed standalone `lighter-<version>-arm64` bootstrap attached. The installer requires the bootstrap; publishing only the archive breaks installation. For the 0.5.0 transition, coordinate merge and immediate publication: the new installer cannot use 0.4.1, which has no standalone bootstrap asset. Publish SHA256 values in the release notes. Never replace a published asset with different bytes under the same version.
7. Confirm the public assets download and pass their hashes, then make the tap PR ready to merge. Homebrew 6 requires testing a formula inside a tap; use an isolated tap checkout and restore its original branch afterward.

The archive contains a manifest sealed inside the signed app. It authenticates the external CLI, kernel, root filesystem and kernel version. Package without AppleDouble sidecars (`COPYFILE_DISABLE=1`); the notarization staple remains an ordinary app resource. A missing or invalid signature, manifest or guest hash must fail before stopping an existing VM.

## After

- A worklog row for the release: what the gates said, the tarball's hash, what was skipped and why.
- The daily driver moved onto the release build.
- `docs/worklog.md` keeps running; the README's numbers change only with the next record.
