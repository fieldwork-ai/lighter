#!/usr/bin/env bash
# Fetch, configure and build the lighter guest kernel.
#
# Runs inside the builder image with /out bind-mounted from the host. Emits an
# uncompressed arm64 `Image` — uncompressed because we boot it directly with no
# bootloader, so kernel self-decompression would be pure added latency on a
# path we are trying to make fast.
set -euo pipefail

KERNEL_VERSION="${KERNEL_VERSION:-6.18.49}"
KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
JOBS="${JOBS:-$(nproc)}"
SRC=/build/linux-${KERNEL_VERSION}
OUT=/out

log() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

log "Building Linux ${KERNEL_VERSION} for arm64 with ${JOBS} jobs"

EXTRACTED=0
if [ ! -d "$SRC" ]; then
	log "Fetching kernel source"
	curl -fsSL --retry 3 \
		"https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/linux-${KERNEL_VERSION}.tar.xz" \
		-o /build/linux.tar.xz
	tar -xf /build/linux.tar.xz -C /build
	rm -f /build/linux.tar.xz
	EXTRACTED=1
fi

cd "$SRC"

# Patches are applied to a pristine tree every time, and the tree lives in a
# named volume across builds — so pristine has to be something the build can
# get back to, not something it assumes. Reverting the previous patches almost
# works, until a patch is EDITED between builds: the old version cannot be
# found to revert, the new one conflicts with its residue, and the build
# produced a kernel from a tree that was neither. Git makes pristine a fact:
# the extracted tarball is committed once, and every build resets tracked
# sources to that commit before patching — object files are untracked and
# survive, so the build stays incremental.
if [ ! -d .git ]; then
	if [ "$EXTRACTED" != 1 ]; then
		echo "error: the source volume predates git-tracked pristine trees and" >&2
		echo "may hold residue of old patches. Remove it and rebuild:" >&2
		echo "    docker volume rm lighter-kernel-src" >&2
		exit 1
	fi
	log "Recording the pristine tree"
	git init -q
	git add -A
	git -c user.name=lighter -c user.email=build@invalid commit -qm pristine
fi
git checkout -q -- .
git clean -qf '*.rej' '*.orig' 2>/dev/null || true

if [ -d /patches ] && ls /patches/*.patch >/dev/null 2>&1; then
	for patch in /patches/*.patch; do
		log "Applying $(basename "$patch")"
		patch -p1 --batch --silent < "$patch"
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
	CONFIG_LIGHTER_FS
	CONFIG_PCI
	CONFIG_VIRTIO_PCI
	CONFIG_VIRTIO_VSOCKETS
	CONFIG_VIRTIO_BALLOON
	CONFIG_PAGE_REPORTING
	CONFIG_SERIAL_AMBA_PL011_CONSOLE
	CONFIG_ARM64_4K_PAGES
	CONFIG_OVERLAY_FS
	CONFIG_EXT4_FS
	CONFIG_BTRFS_FS
	CONFIG_BINFMT_MISC
	CONFIG_BLK_DEV_INITRD
	CONFIG_BRIDGE
	CONFIG_VETH
	CONFIG_NF_NAT
	CONFIG_IP_NF_NAT
	CONFIG_IP_NF_IPTABLES_LEGACY
	CONFIG_IP_NF_TARGET_MASQUERADE
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
# Through a temporary name and a rename, so a copy that fails partway (the
# output directory is the Mac's, through the share) never leaves a truncated
# Image under the name every machine boots from.
cat arch/arm64/boot/Image > "$OUT/Image.tmp"
mv "$OUT/Image.tmp" "$OUT/Image"
cat .config > "$OUT/kernel.config"
printf '%s\n' "$KERNEL_VERSION" > "$OUT/kernel.version"

log "Done: $(du -h "$OUT/Image" | cut -f1) Image for Linux ${KERNEL_VERSION}"
