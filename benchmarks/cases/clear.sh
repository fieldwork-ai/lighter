# Sourced by the setups: `clear_tree <path>` removes a tree left by the last
# repetition. pnpm keeps its store at the root of the share, since its usual
# one is on another filesystem, and hard-links every installed file to it; on
# Podman's shared folder (libkrun, 2026-09-25) `rm -rf` of such a tree leaves
# every hard-linked file behind and fails with "Directory not empty", on every
# pass. A tree that will not go is renamed into `$WORK/.trash`, which the
# harness deletes from the Mac after the case. Setups are untimed, so neither
# enters a number; the timed `rm-rf` case is still one `rm -rf`.
clear_tree() {
	rm -rf "$1" 2>/dev/null || true
	[ -e "$1" ] || return 0
	mkdir -p "$WORK/.trash"
	mv "$1" "$WORK/.trash/$(date +%s)-$$"
}
