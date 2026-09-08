# Final 0.5.1 qualification evidence

Runtime source is 67d787b5910e0dd68b7564d4716fe3b7b0126d1d; signed payload and final portable archive hashes are in final-artifact-equivalence.json.

Both Macs passed fresh installation, real 0.4.1 and 0.5.0 migration, private signed activation/rollback and development update lifecycle checks. The original M1 serial driver retains exit-code 1: its first five stages passed, then the kind harness rejected 0.5.1 because it required 0.5.0. This was before cluster creation. The updated harness accepts an explicit expected version and the separate signed-m1-kind run passes the full two-node test, restart persistence and cleanup. No runtime or guest bytes changed for this retry. The original failed attempt is retained, not rewritten as a pass.

The m1-homebrew check uses the final archive through a local file URL in the real tap, temporarily replacing and then restoring its formula. It verifies repeated post_install, signature/manifest preservation, update ownership and brew test. The M1 is stopped after qualification, its tap has no changes, and its installed Homebrew candidate is 0.5.1.

The rejected-pax-archive records describe an unpublished archive that system tar could extract but the real Rust installer could not. portable-repack.json verifies unchanged signed payloads and successful real installation of the final ustar archive. Private successor fixture metadata is included for reproducibility; the fixture archives and credentials are not published. Cleanup and M5 daily-state checks are recorded separately.

The runtime directory contains frozen-source workspace/hypervisor/gate output and M5 memory/stream/zone checks. Benchmark evidence is adjacent in ../benchmarks. Raw Kubernetes kubeconfig, exported cluster logs and unrelated app inventories are omitted. User paths are normalized; manifest.json hashes the archived representation.
