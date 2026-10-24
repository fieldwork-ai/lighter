<!-- Against `dev`. A security fix goes through a private advisory first: SECURITY.md. -->

## Problem

<!-- What a user sees, or what the code gets wrong. Link the issue. -->

## Root cause

<!-- Why it happens, down to the line that does it. -->

## Fix

<!-- What changes, and why this way rather than another. -->

## Verification

<!-- The test that fails without the fix and passes with it, and what else you ran. -->

- [ ] `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`
- [ ] Touches the machine, the guest or a device: `make test-hv`, `make quick` and the gates it affects, on (Mac model, macOS version):
- [ ] My contribution is dual-licensed under MIT or Apache 2.0 (kernel patches: GPL-2.0)
