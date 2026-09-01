# The guest root filesystem: Alpine, dockerd, and our agent, as an ext4 image.
#
# Alpine rather than buildroot. Buildroot produces a smaller image and a tidier
# story, but Docker's dependency set (iptables, ip6tables, e2fsprogs, xfsprogs,
# containerd, runc) is packaged and tested on Alpine and is a genuine
# build-system project on buildroot. The size difference is tens of megabytes
# of a sparse disk that punches back to nothing; the time difference is days.
FROM alpine:3.21 AS rootfs

# docker-cli is not needed in the guest — the CLI runs on macOS — but dockerd
# pulls its own runtime deps. e2fsprogs is here for the overlay2 storage driver
# and for fsck of the data volume on boot.
#
# `qemu-x86_64` is what makes `--platform linux/amd64` work. Rosetta would be
# faster and is not available to us: it refuses to run outside
# Virtualization.framework, which is a check we are not going to work around.
# See `docs/x86-64.md`.
RUN apk add --no-cache \
	docker-engine \
	containerd \
	runc \
	iptables \
	ip6tables \
	e2fsprogs \
	e2fsprogs-extra \
	util-linux \
	blkid \
	ca-certificates \
	tzdata \
	openrc \
	busybox-openrc \
	qemu-x86_64

COPY init /sbin/lighter-init
COPY lighter-agent /sbin/lighter-agent
RUN chmod +x /sbin/lighter-init /sbin/lighter-agent

# Docker's data root lives on the dedicated disk, mounted at boot. Creating the
# mount point here means init does not have to.
RUN mkdir -p /var/lib/docker /run /mnt/data \
	&& echo "lighter" > /etc/hostname

# ---------------------------------------------------------------------------
# Pack the tree into an ext4 image.
#
# In a container because macOS cannot make an ext4 filesystem, and as a plain
# file because mke2fs -d needs no loop device and therefore no privileges — a
# `docker build` that required --privileged would be a much worse dependency.
FROM debian:bookworm-slim AS image
RUN apt-get update \
	&& apt-get install -y --no-install-recommends e2fsprogs \
	&& rm -rf /var/lib/apt/lists/*

COPY --from=rootfs / /rootfs

# Sized for the tree plus room for logs and growth. Docker images do NOT land
# here — they go on the separate data disk — so this does not grow with what
# the user pulls. It is a sparse file, so the size is a ceiling rather than a
# cost.
#
# The two commands are separate on purpose. They used to be one chain ending in
# `|| [ $? -le 1 ]`, which was meant to forgive `e2fsck` reporting that it had
# fixed something — and forgave `mke2fs` running out of room as well. The build
# then succeeded with a filesystem missing whatever had not fitted, and the
# guest panicked at the first missing binary with `exitcode=0x7f00`, several
# steps into init and nowhere near the cause.
RUN rm -rf /rootfs/proc/* /rootfs/sys/* /rootfs/dev/* \
	&& echo "rootfs tree: $(du -sm /rootfs | cut -f1) MiB in $(find /rootfs | wc -l) files" \
	&& mke2fs -q -t ext4 -d /rootfs -L lighter-root -b 4096 /rootfs.ext4 2G
# `e2fsck` exits 1 when it corrected something, which is not a failure; anything
# above that is.
RUN e2fsck -fp /rootfs.ext4 || [ $? -le 1 ]

FROM scratch
COPY --from=image /rootfs.ext4 /rootfs.ext4
