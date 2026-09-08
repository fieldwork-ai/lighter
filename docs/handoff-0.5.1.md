# 0.5.1 ready-for-merge handoff

The final hybrid runtime is `b78dfeb3063e3ad7af0660050a766032c36f23c0`.
The frozen signed package was built from `11e7ec0f2fd1f3d442f7989027ddef8099b9d36e`.
Later qualification, documentation and formula changes do not alter that payload.
The kernel is 6.18.49; data epoch remains 1.

## Artifacts

| Asset | SHA-256 |
| --- | --- |
| `lighter-0.5.1-arm64.tar.gz` | `414ba7f95f9cde1b895d7aa5982d9d5791882359b49b06c6ad2682454092169b` |
| `lighter-0.5.1-arm64` | `1ae3bbc7bef22197645b6258fdc6b957df180ec542d856405404bf8396f8db7b` |

Apple accepted notarization `874a5d1a-fdd5-4ee8-80ac-54bb2e6aa5fa`.
The draft v0.5.1 release contains exactly these two assets; authenticated
re-downloads match. The local frozen files are in `dist/` on both Macs.
Do not publish the superseded quarter-RAM candidate or private successor fixtures.
Do not rebuild or re-sign during publication.

## Completed qualification

- One full suite on each host, three repetitions per timed case. M5 medians:
  517 ms to Docker, 664 ms to first container; M1: 720 / 905 ms. Historical
  comparisons and workload-specific variation remain explicit.
- The focused deletion comparison is 3.015 vs 2.830 s (+6.5%), inconclusive
  against substantial same-build variation. Noise plausibly contributes;
  a small runtime effect is unresolved. Later informal runs are excluded.
- 343 workspace tests, 24 signed hypervisor tests, all twelve M1 hardware
  gates, M5 memory/idle and speed gates, plus documented hybrid correctness
  and capacity checks. Exact source scope is in the qualification record.
- Final archive on both Macs: fresh install, 0.4.1/0.5.0 migrations,
  staged upgrade/rollback, updater lifecycle and two-node kind with restart.
- M1 Homebrew reinstall via local archive URL, repeated post-install,
  ownership, managed-update refusal, payload hashes, `brew test`, signatures
  and Gatekeeper. The installed tap was restored clean.
- Signed CLI equivalence after signature removal and normalization of the
  validated derived `__LINKEDIT` mapping size; unchanged guest hashes.
  Initial verifier rejection and corrected proof are retained.

See [qualification](release-0.5.1.md), [artifact record](release-0.5.1-artifacts.json)
and [performance report](../benchmarks/RELEASE-0.5.1.md). No full-day soak is claimed.

## Publication after review

1. Review and merge [main PR #5](https://github.com/fieldwork-ai/lighter/pull/5)
   from dev to main. CI must be green on the reviewed head.
2. Confirm v0.5.1 remains draft with exactly the two hashes above. Point its
   target at the reviewed merged main commit and publish it with the prepared
   release notes. Mark it latest. Keep the already-tested asset bytes.
3. Fetch both public v0.5.1 asset URLs and verify their hashes. Exercise the
   public install script in an isolated prefix; do not overwrite a daily VM.
4. Merge [tap PR #5](https://github.com/fieldwork-ai/homebrew-tap/pull/5) only
   after the public archive URL and hash are verified. The tap PR is ready
   for review now, with this explicit publication dependency.
5. Verify the published release and Homebrew formula reference the same archive.

## Host and credential state

The M5 daily installation is the final signed 0.5.1, **stopped**, preserving
16 vCPUs / 16 GiB RAM / 64 GiB disk, its shares and LAN publishing setting.
Configuration hash and data inode/size were checked before and after installation.
M1's Homebrew 0.5.1 is stopped. Neither host has a lighter or Colima VM running.
Paused Apple processes are resumed; the M5 login-service enabled setting is restored.
Temporary test homes/registrations are absent, including those whose cleanup
logged Launch Services -10814. Cached task-owned certificate/API-key/env files
were removed; packaging removed its temporary keychain and restored the keychain list.
Publishing these existing GitHub assets needs no Apple signing credentials.
