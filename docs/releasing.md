# Releasing

A release is a signed, notarized tarball on GitHub, a Homebrew formula that points at it, a tag, and one pull request from `dev` to `main`. This is the order, with the traps found on 0.2.0.

## Before

- The full gate set green on the exact head you will ship: `scripts/gates/run-all.sh` (m1–m8). On a machine where a daily driver runs beside it, `LIGHTER_BENCH_ALLOW_NOISY=1` lets m5 measure; its numbers are not the record then, only the pass. The gates write their measurements to `benchmarks/results/gate-*.csv`, which git ignores; check `git status` under `benchmarks/results/` before the release commit all the same.
- The CI commands themselves, verbatim, not the local habit: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`. 0.2.0's `dev` was red on five clippy warnings a local run had tolerated.
- The daily driver up on the same build for a working day (`make install` or the tarball), because the gates do not cover a Mac that sleeps and wakes, a VPN that changes the resolver, or a week of images.
- The README's numbers regenerated from the record CSVs, never typed: `python3 benchmarks/readme.py --write`. The record is a quiet machine; the M1's comes from `scripts/provision-bench-host.sh`'s host and a runner like `~/remote-record3.sh` there.

## The version

One number, in the workspace `Cargo.toml`: `version` and the four internal crates' `version` pins beside it. 0.2.0's bump missed the pins, the workspace stopped resolving mid-gate, and m7 and m8 failed for a reason that read as a guest problem. Bump all four, and the guest agent's own `guest/agent/Cargo.toml` (and its lockfile) beside them, then `cargo build`, `make guest` for the rootfs the agent lives in, and the gates again on the bumped head — the release commit is the one the gates saw.

## The tarball

```
scripts/package-release.sh 0.3.0
```

Builds release, signs `lighter` with the Developer ID Application identity and the hypervisor entitlement (`cargo build --release` alone strips it, which is what `make sign PROFILE=release` is for during development), submits to `notarytool`, and packs `dist/lighter-<version>-arm64.tar.gz` with the kernel, the rootfs and the entitlements. `--skip-notarize` is for checking the packaging, not for shipping. The tarball must come from the head the gates passed: 0.2.0's first tarball was built one commit early and withdrawn.

Then, by hand:

1. Install the tarball somewhere fresh and run `lighter doctor`, `lighter start`, an amd64 container, `lighter stop`.
2. A GitHub release `v<version>` with the tarball attached; note its sha256.
3. `packaging/lighter.rb`: the `url` and `sha256`, then the same file into `fieldwork-ai/homebrew-tap` on a branch and a pull request there (its `main` needs a review). To test the formula before it merges, Homebrew 6 refuses `brew install ./lighter.rb`; check the branch out inside the tap clone instead, `cd $(brew --repository fieldwork-ai/tap) && git checkout origin/<branch>`, then `brew install fieldwork-ai/tap/lighter` from a machine that never had it, and put the clone back on `main` afterwards. The M1's clone had a conflicted index from an earlier attempt; `git reset --hard origin/main` there is safe, it is a throwaway.
4. Tag the release commit `v<version>` on `dev` and push the tag.
5. One pull request `dev` → `main`, the plan or the worklog rows for the release in its body. `main` is protected and is only ever moved by that pull request.

## After

- A worklog row for the release: what the gates said, the tarball's hash, what was skipped and why.
- The daily driver moved onto the release build.
- `docs/worklog.md` keeps running; the README's numbers change only with the next record.
