# Builds the coherence prober as a static aarch64-musl binary.
#
# Same constraints as the agent: no dynamic loader in the initramfs, musl rather
# than glibc, and `--locked` because this binary is the arbiter of whether the
# filesystem is correct and must not vary with the day it was built.
FROM rust:1-alpine AS build

RUN apk add --no-cache musl-dev file

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

RUN file target/release/lighter-fstest | grep -q "interpreter" \
	&& (echo "the prober needs a dynamic loader; it would not run here" >&2 && exit 1) \
	|| file target/release/lighter-fstest

FROM scratch
COPY --from=build /src/target/release/lighter-fstest /lighter-fstest
