#!/bin/sh
# The failing shape: install, wipe node_modules, install again.
set -u
cd "$WORK/npm"
R="$WORK/pnpm-twice.out"
: > "$R"
rm -rf node_modules "$WORK/.pnpm-store"
echo "=== install 1" >> "$R"
pnpm install --frozen-lockfile --ignore-scripts >> "$R" 2>&1
echo "install1 exit=$?" >> "$R"
echo "store files after 1: $(find "$WORK/.pnpm-store/v10/files" -type f 2>/dev/null | wc -l)" >> "$R"
rm -rf node_modules
echo "=== install 2" >> "$R"
pnpm install --frozen-lockfile --ignore-scripts >> "$R" 2>&1
echo "install2 exit=$?" >> "$R"
echo "TIME_MS 1"
