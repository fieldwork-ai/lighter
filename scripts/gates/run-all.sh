#!/usr/bin/env bash
# Every landed milestone gate, timed.
#
# The timing is the point of running them through here rather than as a list of
# make targets. A gate nobody runs is not a gate, and the way a suite stops
# being run is by getting slower a minute at a time with nobody able to say
# which minute. So each one reports what it cost, and the total is printed at
# the end where it cannot be missed.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

GATES=(
	"m1  first boot:m1-boot.sh"
	"m2  core devices:m2-devices.sh"
	"m3  network:m3-network.sh"
	"m3  vsock:m3-vsock.sh"
	"m3  docker:m3-docker.sh"
	"m4  filesystem:m4-fs.sh"
	"m5  speed:m5-speed.sh"
	"m6  memory:m6-memory.sh"
	"m7  x86-64:m7-amd64.sh"
	"m8  daily driver:m8-daily.sh"
)

only="${1:-}"
results=()
failed=0
total_start=$SECONDS

for entry in "${GATES[@]}"; do
	name="${entry%%:*}"
	script="${entry##*:}"
	[ -z "$only" ] || case "$script" in *"$only"*) ;; *) continue ;; esac

	printf '\n\033[1m==> %s\033[0m\n' "$name"
	start=$SECONDS
	if "scripts/gates/$script" </dev/null; then
		verdict=$'\033[32mpass\033[0m'
	else
		verdict=$'\033[31mFAIL\033[0m'
		failed=1
	fi
	results+=("$(printf '%s|%s|%s' "$name" "$verdict" "$((SECONDS - start))")")
done

printf '\n\033[1m==> Gates\033[0m\n'
for row in "${results[@]}"; do
	IFS='|' read -r name verdict seconds <<<"$row"
	printf '  %b  %-24s %4ds\n' "$verdict" "$name" "$seconds"
done
printf '  %-30s %4ds\n' "total" "$((SECONDS - total_start))"

exit "$failed"
