#!/bin/sh
# Copying a large tree within the share: the create-and-write storm, on a tree
# that is real rather than synthesized. Every file is read and written across
# the boundary, and each one costs a lookup, a create, a write and a close.
#
# It is also the case that found the readdir bug: `cp -a` remembers every
# `(dev, ino)` it has copied and hard-links anything that repeats, so a listing
# that returns an entry twice fails here and passes everywhere else.
set -eu
cd "$WORK/npm"
cp -a node_modules node_modules_copy
