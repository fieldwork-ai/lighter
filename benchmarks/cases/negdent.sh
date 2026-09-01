#!/bin/sh
# Minimal repro: does a cached negative dentry survive the file being created
# by rename/link? Results written to the share so the host can read them.
set -u
cd "$WORK"
rm -rf negdent; mkdir negdent
R="negdent/RESULT"
: > "$R"
stat negdent/target >/dev/null 2>&1 && echo "pre-exists!?" >> "$R"
echo hello > negdent/target.tmp
mv negdent/target.tmp negdent/target
if stat negdent/target >/dev/null 2>&1; then echo "RENAME-VISIBLE yes" >> "$R"; else echo "RENAME-VISIBLE NO" >> "$R"; fi
stat negdent/ltarget >/dev/null 2>&1
echo hello > negdent/lsource
ln negdent/lsource negdent/ltarget
if stat negdent/ltarget >/dev/null 2>&1; then echo "LINK-VISIBLE yes" >> "$R"; else echo "LINK-VISIBLE NO" >> "$R"; fi
# Repeat both 200 times to catch a race rather than a determinism
fail=0
i=0
while [ $i -lt 200 ]; do
  stat "negdent/r$i" >/dev/null 2>&1
  echo x > "negdent/r$i.tmp"
  mv "negdent/r$i.tmp" "negdent/r$i"
  stat "negdent/r$i" >/dev/null 2>&1 || fail=$((fail+1))
  i=$((i+1))
done
echo "RENAME-RACE misses=$fail/200" >> "$R"
fail=0
i=0
while [ $i -lt 200 ]; do
  stat "negdent/l$i" >/dev/null 2>&1
  echo x > "negdent/l$i.src"
  ln "negdent/l$i.src" "negdent/l$i"
  stat "negdent/l$i" >/dev/null 2>&1 || fail=$((fail+1))
  i=$((i+1))
done
echo "LINK-RACE misses=$fail/200" >> "$R"
echo "TIME_MS 1"
