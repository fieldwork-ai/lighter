# Build environment for the lighter guest kernel.
#
# Runs natively on an aarch64 container host, so there is no cross-compiler and
# no CROSS_COMPILE prefix to get wrong. The image is pinned by digest-free tag
# on purpose: kernel builds are reproducible from the source tarball hash, not
# from the toolchain, and pinning Debian here would just add a stale-base chore.
FROM debian:bookworm-slim

# dwarves is pahole, which makes BTF from the build's DWARF; python3 is for
# the libbpf headers resolve_btfids is built against.
RUN apt-get update && apt-get install -y --no-install-recommends \
	bc \
	bison \
	build-essential \
	ca-certificates \
	cpio \
	curl \
	dwarves \
	flex \
	git \
	gzip \
	kmod \
	libelf-dev \
	libssl-dev \
	patch \
	python3 \
	rsync \
	xz-utils \
	&& rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY build-kernel.sh /usr/local/bin/build-kernel
COPY lighter.config /config/lighter.config
COPY patches /patches
RUN chmod +x /usr/local/bin/build-kernel

ENTRYPOINT ["/usr/local/bin/build-kernel"]
