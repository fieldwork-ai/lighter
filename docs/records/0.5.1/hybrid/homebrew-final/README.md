# M1 Homebrew final artifact check

The final archive was reinstalled over the earlier unpublished 0.5.1 candidate.
This is a reinstall, not a fresh 0.5.0 Homebrew upgrade. The direct signed
migration checks on both hosts cover actual 0.5.0 → 0.5.1 data preservation.

The candidate formula used a local file URL because the release remains draft.
Post-install was repeated twice; signed payload hashes, ownership metadata,
refusal of direct managed updates, update check, `brew test`, signatures and
Gatekeeper all passed. The installed tap formula was restored and checked clean.
Public release download/hash verification remains a post-publication step.
