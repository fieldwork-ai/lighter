# M5 final hardware qualification

Source `1b0caabf34fbe359f11cd47cc87ce45dc8583682`; runtime code `66897086c3702b75d79cb6fcfed7736842b80780`. All twelve gates passed with identical artifact hashes before and after. Workspace and hypervisor logs retain individual results. The guard used quiet preflight and continuous competing-VM checks; public process paths are reduced to executable basenames.

Earlier qualification attempts are retained privately: a VM-ownership registration error, an intentionally interrupted toolchain correction, and a passing gate sequence whose benchmark fingerprint differed because its initial Cargo invocation selected different package features. The CLI and guest were unchanged in that last attempt. These records are the corrected repeat with consistent build invocations and stable fingerprints. Gate timings validate functionality and are not the primary release performance record.
