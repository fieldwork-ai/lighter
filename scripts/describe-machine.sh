#!/usr/bin/env bash
# One line saying what machine this is, for the benchmark report: model,
# chip, cores, memory, macOS. Generated, never typed, so two machines' result
# sets can never be confused for one another.
set -eu
model="$(sysctl -n hw.model)"
name="$(system_profiler SPHardwareDataType 2>/dev/null | awk -F': ' '/Model Name/ {print $2}')"
chip="$(sysctl -n machdep.cpu.brand_string)"
perf="$(sysctl -n hw.perflevel0.physicalcpu 2>/dev/null || echo 0)"
eff="$(sysctl -n hw.perflevel1.physicalcpu 2>/dev/null || echo 0)"
cores="$(sysctl -n hw.physicalcpu)"
mem_gb="$(( $(sysctl -n hw.memsize) / 1024 / 1024 / 1024 ))"
os="$(sw_vers -productVersion)"
printf '%s (%s), %s, %s cores (%sP+%sE), %s GB, macOS %s\n' \
	"${name:-Mac}" "$model" "$chip" "$cores" "$perf" "$eff" "$mem_gb" "$os"
