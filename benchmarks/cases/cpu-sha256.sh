#!/bin/sh
# Pure computation, no disk and no network: a gigabyte of zeros through
# sha256sum. Native it is a few hundred milliseconds of one core; under a
# translator it is the translator's price on straight-line code, with nothing
# of the runtime's filesystem or network in the number. That is what the
# x86-64 table wants beside the install cases, which are mostly waiting.
set -eu
if command -v sha256sum >/dev/null 2>&1; then
	head -c 1073741824 /dev/zero | sha256sum > /dev/null
else
	head -c 1073741824 /dev/zero | shasum -a 256 > /dev/null
fi
