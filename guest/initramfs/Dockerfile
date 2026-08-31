# Assembles the boot initramfs and writes the cpio archive to stdout.
#
# busybox:musl ships a genuinely static binary, which matters: the initramfs has
# no dynamic loader and no libc, so anything dynamically linked is a PID 1 that
# dies with an unhelpful "No such file or directory".
FROM busybox:musl AS busybox

FROM debian:bookworm-slim AS build
RUN apt-get update \
	&& apt-get install -y --no-install-recommends cpio gzip file \
	&& rm -rf /var/lib/apt/lists/*

WORKDIR /rootfs
COPY --from=busybox /bin/busybox /rootfs/bin/busybox
COPY init /rootfs/init
COPY udhcpc.script /rootfs/usr/share/udhcpc/default.script

# The initramfs has no dynamic loader, so PID 1 must need none. The test is the
# absence of an ELF interpreter, not the word "static": busybox:musl is
# `static-pie linked`, which the kernel loads fine and a naive grep rejects.
# Checking here means a bad binary is a build error rather than an unhelpful
# "No such file or directory" from a guest with no console yet.
RUN file /rootfs/bin/busybox | grep -q "interpreter" \
	&& (echo "busybox needs a dynamic loader; the initramfs would not boot" >&2 && exit 1) \
	|| file /rootfs/bin/busybox

RUN chmod +x /rootfs/init /rootfs/bin/busybox /rootfs/usr/share/udhcpc/default.script \
	&& mkdir -p /rootfs/proc /rootfs/sys /rootfs/dev /rootfs/tmp /rootfs/root /rootfs/etc \
	&& mknod -m 622 /rootfs/dev/console c 5 1 \
	&& mknod -m 666 /rootfs/dev/null c 1 3 \
	&& mknod -m 666 /rootfs/dev/tty c 5 0 \
	&& mknod -m 444 /rootfs/dev/urandom c 1 9

# newc is the only format the kernel's initramfs unpacker accepts.
RUN cd /rootfs && find . | cpio -o -H newc --quiet | gzip -9 > /initramfs.cpio.gz

# Exported with `docker build --output type=local`, so this stage holds exactly
# the artifact and nothing else.
FROM scratch
COPY --from=build /initramfs.cpio.gz /initramfs.cpio.gz
