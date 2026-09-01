#!/bin/sh
# strace the second install; keep only the syscalls about the store.
set -u
cd "$WORK/npm"
R="$WORK/strace"
rm -rf "$R"; mkdir -p "$R"
command -v strace >/dev/null 2>&1 || { apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq strace >/dev/null 2>&1; } || apk add strace >/dev/null 2>&1
rm -rf node_modules "$WORK/.pnpm-store"
pnpm install --frozen-lockfile --reporter=silent --ignore-scripts > "$R/first.out" 2>&1
echo "first exit=$?" >> "$R/first.out"
rm -rf node_modules
strace -ff -o "$R/t" -e trace=openat,open,rename,renameat2,link,linkat,unlink,unlinkat,statx,newfstatat -qq \
  pnpm install --frozen-lockfile --reporter=silent --ignore-scripts > "$R/second.out" 2>&1
echo "second exit=$?" >> "$R/second.out"
echo "TIME_MS 1"
