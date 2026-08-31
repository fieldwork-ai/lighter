# Builds the guest agent as a static aarch64-musl binary.
#
# Static because the initramfs has no dynamic loader: the same constraint that
# governs busybox governs this. musl rather than glibc because glibc's static
# linking still wants NSS shared objects at runtime for anything that resolves a
# name, which is a trap that only fires later.
FROM rust:1-alpine AS build

RUN apk add --no-cache musl-dev file

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# The image is already aarch64 (we build on Apple Silicon), so the host target
# is the guest target and there is no cross-compilation to arrange.
#
# `--locked` rather than a best-effort fallback: this binary ends up inside a
# guest image, and "it built with whatever was newest that day" is not a
# property we want a rootfs to have.
RUN cargo build --release --locked

# Prove it needs no loader, here, where the failure is a build error rather than
# a guest that cannot exec its agent. The test is the absence of an ELF
# interpreter, not the presence of the word "static": a musl release build is
# `static-pie linked`, which the kernel loads fine and a naive grep rejects.
# (The same check, for the same reason, guards busybox in the initramfs.)
RUN file target/release/lighter-agent | grep -q "interpreter" \
	&& (echo "agent needs a dynamic loader; it would not run in the initramfs" >&2 && exit 1) \
	|| file target/release/lighter-agent

FROM scratch
COPY --from=build /src/target/release/lighter-agent /lighter-agent
