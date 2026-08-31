#!/usr/bin/env bash
# Fetch, configure and build the lighter guest kernel.
#
# Runs inside the builder image with /out bind-mounted from the host. Emits an
# uncompressed arm64 `Image` — uncompressed because we boot it directly with no
# bootloader, so kernel self-decompression would be pure added latency on a
# path we are trying to make fast.
set -euo pipefail

KERNEL_VERSION="${KERNEL_VERSION:-6.12.51}"
KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
JOBS="${JOBS:-$(nproc)}"
SRC=/build/linux-${KERNEL_VERSION}
OUT=/out

log() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

log "Building Linux ${KERNEL_VERSION} for arm64 with ${JOBS} jobs"

if [ ! -d "$SRC" ]; then
	log "Fetching kernel source"
	curl -fsSL --retry 3 \
		"https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/linux-${KERNEL_VERSION}.tar.xz" \
		-o /build/linux.tar.xz
	tar -xf /build/linux.tar.xz -C /build
	rm -f /build/linux.tar.xz
fi

cd "$SRC"

# Patches are applied to a pristine tree every time, and the tree lives in a
# named volume across builds — so each one is reverted first. `--forward`
# alone is not enough: a partially applied patch would be neither reverted nor
# reapplied, and the build would silently produce the wrong kernel.
if [ -d /patches ] && ls /patches/*.patch >/dev/null 2>&1; then
	for patch in /patches/*.patch; do
		log "Applying $(basename "$patch")"
		if patch -p1 -R --dry-run --silent < "$patch" >/dev/null 2>&1; then
			patch -p1 -R --silent < "$patch"
		fi
		patch -p1 --silent < "$patch"
	done
fi

log "Configuring (defconfig + lighter fragment)"
make ARCH=arm64 defconfig
./scripts/kconfig/merge_config.sh -m -O . .config /config/lighter.config
# merge_config leaves the merged result needing a pass to settle dependencies;
# olddefconfig takes the default for anything newly reachable rather than
# prompting, which would hang a non-interactive build.
make ARCH=arm64 olddefconfig

# A silently dropped option is the failure mode that costs a day: the kernel
# builds, boots, and then has no virtio. Verify the load-bearing ones survived
# dependency resolution rather than trusting the merge.
log "Verifying critical options survived dependency resolution"
required=(
	CONFIG_VIRTIO_MMIO
	CONFIG_VIRTIO_BLK
	CONFIG_VIRTIO_NET
	CONFIG_VIRTIO_FS
	CONFIG_VIRTIO_VSOCKETS
	CONFIG_VIRTIO_BALLOON
	CONFIG_PAGE_REPORTING
	CONFIG_SERIAL_AMBA_PL011_CONSOLE
	CONFIG_ARM64_4K_PAGES
	CONFIG_OVERLAY_FS
	CONFIG_EXT4_FS
	CONFIG_BINFMT_MISC
	CONFIG_BLK_DEV_INITRD
	CONFIG_BRIDGE
	CONFIG_VETH
	CONFIG_NF_NAT
	CONFIG_PACKET
	CONFIG_INET
	CONFIG_NFT_COMPAT
)
missing=0
for opt in "${required[@]}"; do
	if ! grep -qE "^${opt}=y" .config; then
		echo "  MISSING: ${opt}" >&2
		missing=1
	fi
done
if [ "$missing" -ne 0 ]; then
	echo "error: required kernel options were dropped during olddefconfig" >&2
	exit 1
fi
echo "  all ${#required[@]} required options present"

log "Building Image"
make ARCH=arm64 -j"${JOBS}" Image

mkdir -p "$OUT"
cp arch/arm64/boot/Image "$OUT/Image"
cp .config "$OUT/kernel.config"
printf '%s\n' "$KERNEL_VERSION" > "$OUT/kernel.version"

log "Done: $(du -h "$OUT/Image" | cut -f1) Image for Linux ${KERNEL_VERSION}"
