"""compare.py <prove -v log> <known list> [--write]: pjdfstest's failures against the known list.

A failure is a `not ok` that is not marked TODO (prove counts those as
passes: they are Linux's own known deviations). The list is one
`<test file> <subtest>` per line. Exit 1 if anything fails that the list
does not have; what it has that now passes is named, to be taken off.
With --write, the list is rewritten from the log instead.
"""

import re
import sys


def failures(log):
    found, current = set(), None
    for line in open(log, errors="replace"):
        file = re.match(r"/opt/pjdfstest/tests/(\S+) \.\.", line)
        if file:
            current = file.group(1)
            continue
        failed = re.match(r"not ok (\d+)", line)
        if failed and current and "# TODO" not in line:
            found.add((current, int(failed.group(1)), line.strip()[:160]))
    return found


def main():
    log, known_path = sys.argv[1], sys.argv[2]
    found = failures(log)
    if "--write" in sys.argv:
        with open(known_path, "w") as out:
            out.write("# pjdfstest subtests a shared folder fails by design; see m21-posix.sh.\n")
            for file, n, _ in sorted(found):
                out.write(f"{file} {n}\n")
        print(f"wrote {len(found)} known failures")
        return 0
    known = set()
    for line in open(known_path):
        if line.strip() and not line.startswith("#"):
            file, n = line.split()
            known.add((file, int(n)))
    if not any(re.match(r"/opt/pjdfstest/tests/", l) for l in open(log, errors="replace")):
        print("pjdfstest did not run")
        return 1
    new = sorted((f, n, line) for f, n, line in found if (f, n) not in known)
    fixed = sorted(known - {(f, n) for f, n, _ in found})
    if new:
        print(f"{len(new)} pjdfstest subtests fail that did not ({len(found)} failing, {len(known)} known)")
        for f, n, line in new:
            print(f"new {f} {n}: {line}")
    else:
        print(f"pjdfstest fails only what it failed before: {len(found)} subtests, all on the known list")
    for f, n in fixed:
        print(f"fixed {f} {n} now passes; take it off the known list")
    return 1 if new else 0


sys.exit(main())
