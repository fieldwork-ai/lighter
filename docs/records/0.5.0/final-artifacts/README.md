# Exact 0.5.0 artifact qualification

Archive `4da3fca84dbaf6d1ede81d682efbb52df64db438cfeef9679d3e60eb05e8df52`
and bootstrap `594b09e923358787fed3c67788fa95a6d76f172b8132daae3e55d64982b9438a`
were built from `fe9370566ae082ff2c0d5b3f642354f6ec388de9` after the benchmark
record. The final unsigned executable and guest payload equal the qualified
M5 runtime and payload. Apple notarization, staple and Gatekeeper checks pass.

- `m5/` is the complete seven-stage run, with exit status 0.
- `m1-before-kind/` retains five passing stages, including real login-service
  activation/rollback. Its original exit status 1 remains: the wrapper then
  stopped before cluster creation because a private kind executable was absent.
- `m1-continuation/` validates identical source and all input hashes before
  retaining that passing prefix and completing signed kind and development
  lifecycle tests. It passes with exit status 0. The provisioned kind binary
  is the exact M5 v0.33.0 executable, SHA256
  `0c8c7dbe5e23594a198b786c4bc13dacc101fa6196b0cb0b23a1ca44e61f4b4f`.
- `m1-input-preflight-failed/` stopped before VM tests when private future
  fixtures were missing. They were copied and hash-verified before retrying.
- `m1-homebrew/` records the actual upgrade, repeated postinstall, exact-byte
  and signed-manifest verification, Gatekeeper, and direct-update refusal.
  `m1-brew-not-started/` records the earlier unmet test prerequisite.

The public test scripts define the assertions. The development lifecycle stage
uses an ad-hoc signed managed layout; release trust is exercised by the exact
archive migrations and activation/rollback stages. Private 0.4.2 and future
fixtures are test inputs, never public releases. Kubeconfigs and their keys are
excluded. Original failed statuses and successful stage prefixes are preserved.

Launch Services reported some downloaded/unregistered app paths as not found
during cleanup. Independent registry checks found no remaining entries for
either completed staged-upgrade test home. No runtime fix was needed for the
two M1 harness provisioning corrections, and release bytes were not rebuilt.
