# Complete directories: answering from what the guest made (proposal)

Status: step 1 (missing names) built as kernel patch 0044, 2026-09-26; step 2 (listings) proposed. Follows kernel patch 0043, which keeps the data of files the guest wrote.

## Where the time still goes

A container that reads the tree another container has just written through the share (ripgrep over `node_modules` straight after `npm ci`, on the M1) took 5.7 s against 0.13 s warm. Patch 0043 took that to 1.2 s: no file is read from the Mac any more. What is left is metadata, counted by the share server in the cold pass:

| request | count | what asks |
|---|---:|---|
| LOOKUP answering "no such file" | 37,161 | ripgrep looking for `.gitignore`, `.ignore`, `.rgignore` and `.git` in every directory |
| READDIR | 15,543 | ripgrep listing every directory |
| READ | 0 | (was 77,000 before 0043) |

Neither is wrong. Linux's FUSE client never knows that a directory contains only what it can see, so a name it has no dentry for is a round trip, and a listing is always the server's. Most tools that walk a tree ask the same questions: a build looking for config files in every directory, a test runner globbing, an editor's file watcher starting.

## The idea

A directory the guest made itself (its MKDIR reply) starts empty, and while the guest is the only one changing it, the guest's dcache holds every entry in it. Such a directory is *complete*: a name with no dentry does not exist, and the listing is the dcache's. Ceph's kernel client does exactly this (`CEPH_I_COMPLETE`, `__dcache_readdir`, `ceph_d_prune`), for the same reason.

- **Set** on a directory created by this guest's MKDIR, when the server speaks the create dialect and invalidation is pushed (the notification queue is live).
- **Negative lookups** in a complete directory are answered locally: a negative dentry with the negative-entry timeout, no request.
- **READDIR** of a complete directory iterates its children in the dcache, positive dentries only, as `dcache_readdir` does, with offsets that stay valid across `seekdir`/`telldir` (the cursor-dentry scheme libfs uses).
- **Cleared**, and the directory falls back to asking, on anything that could make the dcache incomplete:
  - a notification about the directory or any entry in it (a change on the Mac, from FSEvents);
  - a lost-notification reset (patch 0028), for every directory;
  - a child dentry pruned from the dcache under memory pressure (`d_prune`, as Ceph);
  - a rename moving an entry in from a directory that is not itself complete;
  - an error from the server on any operation in the directory.

A directory that has been cleared stays cleared until the guest makes it again. Nothing is ever made complete by listing it: a listing from the server is a snapshot the Mac can change under it without the guest being told in time.

## What it would buy

Both counts above go to zero for a tree the guest wrote, so the cold pass should land near the warm one (0.13–0.24 s against 1.2 s now and OrbStack's 1.1 s). Installs gain too: a package manager checks for names before it creates them, and every one of those is a negative LOOKUP today.

## Step 1, measured

Patch 0044 answers missing names in a complete directory; listings still come from the host. On the M1, in the harness (npm-install, then ripgrep over the tree it wrote):

| | 0.10.0 | 0043 | 0043 + 0044 |
|---|---:|---:|---:|
| ripgrep, first pass | 5,794 ms | 1,219 ms | 637 ms |
| ripgrep, warm | 124–160 ms | 129–158 ms | 123–133 ms |
| npm-install (five) | 10.5–11.3 s | 10.4–11.1 s | 10.1–10.5 s |

The first pass now asks the host for 15,452 listings, 22 lookups and one read; the install asks for no lookups at all. Gate m4 passed five times in a row with the new coherence check (a name the guest was told is missing, then made on the host, is found; a file deleted on the host from a guest-made directory is gone).

That check first failed twice in eight runs, and the fault was the test's: the share acknowledges a guest's create at once and applies it a moment later, and the host half deleted the file before the create had landed, so the file really did exist afterwards. The host half now waits for the file before deleting it. It is an edge case of the share itself, older than any of this: a program on the Mac deleting a file within milliseconds of a container creating it can have the delete undone by the create landing after it.

## Risks, and how they are held

- **A missed invalidation now hides a file, not just stale data.** The same notifications 0043 relies on carry it, and a lost one resets everything; the share server's `IgnoreSelf` stream is what reports Mac edits, and m4's coherence stage exercises every kind. New coherence checks: a file the Mac creates in a directory the guest made is found by name and listed; a file the Mac deletes from it is gone from the listing.
- **Readdir offsets.** Served from the dcache they must survive a concurrent create or unlink as `dcache_readdir` does, and never collide with the server's offsets: a directory changes mode only between opens, never under an open handle.
- **Memory.** Nothing new is kept: it answers from dentries the dcache holds anyway, and gives up the moment one is pruned.

## Verification

Gate m4 in full with the new coherence checks; the harness's share cases (`ripgrep`, `find-walk`, `copy-tree`, the installs) on the M1 and M5 against the current kernel; a stress of the Mac creating and deleting in a guest-made directory while the guest lists it, counting any listing that disagrees with the host after the notification lands.
