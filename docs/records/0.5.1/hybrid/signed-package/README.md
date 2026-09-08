# Final hybrid signed package

Build source `11e7ec0`; runtime `b78dfeb`. Apple accepted notarization, and
staple validation, Gatekeeper, signatures, sealed payload and real archive
installation pass. `artifact-equivalence.json` identifies the exact bytes.

The original signature-removal-only comparison rejected one byte at offset
1633: the derived `__LINKEDIT.vmsize`. The larger Developer ID signature
crosses a 16 KiB mapping boundary; stripping the signature preserves that
mapping size. The corrected verifier validates the original read-only,
section-free segment and its rounded size, strips the signature, normalizes
only this derived size, and requires all remaining bytes to match. Kernel
and rootfs hashes match without normalization. Both the initial rejection
and successful verification are retained; this is not a runtime code change.
