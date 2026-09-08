# 0.5.1 publication handoff

[Release qualification](release-0.5.1.md) is complete. The
[artifact manifest](release-0.5.1-artifacts.json) owns the exact filenames,
hashes, source revisions and notarization identity. Only its two final assets
belong in the draft release; do not rebuild or re-sign them during publication.

## Publication after review

1. Review and merge [main PR #5](https://github.com/fieldwork-ai/lighter/pull/5)
   from dev to main. CI must be green on the reviewed head.
2. Confirm v0.5.1 remains draft with exactly the two hashes in the artifact manifest. Point its
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
