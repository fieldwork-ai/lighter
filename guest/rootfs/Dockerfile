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
	busybox-openrc

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

# Sized for the tree plus room for logs and a little growth. Docker images do
# NOT land here — they go on the separate data disk — so this does not need to
# grow with what the user pulls.
RUN rm -rf /rootfs/proc/* /rootfs/sys/* /rootfs/dev/* \
	&& mke2fs -q -t ext4 -d /rootfs -L lighter-root -b 4096 /rootfs.ext4 1G \
	&& e2fsck -fp /rootfs.ext4 || [ $? -le 1 ]

FROM scratch
COPY --from=image /rootfs.ext4 /rootfs.ext4
