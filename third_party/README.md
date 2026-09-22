# Vendored crates

`v4l2r` (MIT, Alexandre Courbot) and `virtio-media` (BSD-3-Clause, the ChromiumOS authors), from crates.io at the versions in their `Cargo.toml`, patched in through the workspace `[patch.crates-io]`. They are here for one reason: both compile only on Linux as published (epoll, eventfd, memfd), and lighter needs their types, ioctl encoders and the virtio-media device framework on macOS, where it is the device rather than a client of one. The patches are `cfg(target_os = "linux")` on the modules that drive a real kernel device, and one cast in `v4l2r`'s buffer timestamp. Nothing else is changed; a bump is a fresh copy plus those lines.

- `virtio-media`: the "process_events called but no event was pending" line is `debug!`, not `warn!`: lighter calls it after every command to collect what a synchronous decoder produced, and most commands produce nothing.
