# Sourced by the setups: `clear_tree <path>` removes a tree left by the last
# repetition, passing again until it is gone. On Podman's shared folder a
# single `rm -rf` of a pnpm-installed node_modules fails with "Directory not
# empty" (2026-09-25, libkrun, reproduced from a clean folder), which failed
# every case after the first pnpm install. Setups are untimed, so the extra
# passes enter no number; the timed `rm-rf` case is still one `rm -rf`.
clear_tree() {
	i=0
	while [ -e "$1" ] && [ "$i" -lt 5 ]; do
		rm -rf "$1" 2>/dev/null || true
		i=$((i + 1))
	done
	[ ! -e "$1" ] || { echo "clear_tree: $1 still there after $i passes" >&2; rm -rf "$1"; }
}
