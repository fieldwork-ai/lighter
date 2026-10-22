# Contributing to lighter

Thank you for helping. A bug report with a way to reproduce it is as valuable as a fix, and a good one is most of the work: #95 and #98 were each fixed in days because the report named the machine, the logs and the measurements.

## Reporting a bug

Open an issue with the bug report form. It asks for what lighter needs to see the problem: what you ran, what happened, `lighter doctor`, the relevant part of `~/.lighter/machine.log`, your Mac's model and macOS version. A security problem is different: report it privately, as [SECURITY.md](SECURITY.md) describes, not in an issue.

## Changing lighter

Open pull requests against `dev`. `main` moves only through a release's pull request from `dev` ([docs/releasing.md](docs/releasing.md)).

Before you push, run what CI runs:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
ulimit -S -n 10240 && cargo test --all
```

CI runs on GitHub's macOS runners, which are virtual machines that cannot start one of their own, so nothing there boots a guest. Changes that touch the machine, the guest or a device also need the tests that do, on Apple Silicon: `make test-hv` for the tests that drive the hypervisor, `make quick` for a real boot, and the gates that cover what you changed (`make gates`, or `scripts/gates/<gate>.sh` for one). Say in the pull request which you ran and on what Mac.

A pull request describes the problem it solves, its root cause, the fix, and how it was verified; the template has those headings. A fix comes with a test that fails without it. A test that fails sometimes is a bug to fix, in the same pull request if you meet it, never one to retry.

Comments in lighter say why, not what: the constraint the code answers to, the alternative that did not work. Most changes need none.

## Kernel patches

The guest's kernel patches (`guest/kernel/patches`) change Linux, so they are under the licence of the files they change, GPL-2.0 for most of the kernel. Write them as patches for upstream, with a `Signed-off-by:` line certifying the [Developer Certificate of Origin](https://developercertificate.org), so that they can be sent to the kernel's maintainers.

## Licensing

lighter is licensed under the [MIT License](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in lighter by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions. Kernel patches are the exception above.

A new dependency must be under a licence `licenses/about.toml` accepts, which CI checks, and a new C or C++ library linked into what a release ships belongs in `licenses/native.json`, so that the release's notices name it.
