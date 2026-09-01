// Per-operation latency, on the share and on the guest's own disk, paired.
//
// The suite's other cases are workloads: they run a package manager and report
// how long it took, which is the number anyone cares about and is also the one
// that takes six minutes and moves by two hundred milliseconds between runs.
// This is the instrument rather than the result. It does one kind of syscall at
// a time and reports microseconds.
//
// # Why every case is measured twice
//
// A number from a busy machine is not a measurement. The usual answer is to
// wait for a quiet machine, which is often not available, or to take more
// samples, which does not help — interference is not zero-mean noise, it is a
// bias, and averaging a biased estimator gives a precise wrong answer.
//
// So each case runs against the share *and* against the guest's own disk, in
// the same boot, alternating between them. Both feel whatever else the host is
// doing, so the difference between them does not: `create+close` on ext4
// inside the guest is a control for `create+close` through the share, and the
// gap between the two is the thing this file exists to measure. Absolute
// figures from a contended machine are worth little; the paired difference
// survives it.
//
//   ONLY=create+close   one case, so a server-side histogram can be attributed
//   OPS=3000            operations per sample
//   ROUNDS=3            alternations between share and local, medians reported
//   PARALLEL=16         batch size for the concurrent case
const fs = require("node:fs");
const path = require("node:path");

const reps = Number(process.env.OPS || 3000);
const only = process.env.ONLY;
const rounds = Number(process.env.ROUNDS || 3);
const PARALLEL = Number(process.env.PARALLEL || 16);

// The share, and a directory on whatever the guest boots from.
const places = [
  ["", path.join(process.env.DIR, "op-latency")],
  ["@local", "/tmp/op-latency"],
];

function fresh(dir) {
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { recursive: true });
}

function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[(sorted.length - 1) >> 1];
}

// Alternates the two places round by round, so a host that gets busier partway
// through makes both slower rather than one.
function bench(name, run, setup) {
  if (only && only !== name) return;
  const samples = new Map(places.map(([suffix]) => [suffix, []]));
  for (let round = 0; round < rounds; round++) {
    for (const [suffix, dir] of places) {
      fresh(dir);
      if (setup) setup(dir);
      const started = process.hrtime.bigint();
      run(dir);
      samples.get(suffix).push(Number(process.hrtime.bigint() - started) / 1000 / reps);
    }
  }
  for (const [suffix] of places) {
    console.log(`US ${name}${suffix} ${median(samples.get(suffix)).toFixed(2)}`);
  }
}

const each = (fn) => (dir) => {
  for (let i = 0; i < reps; i++) fn(dir, i);
};

const touch = (prefix) => (dir) => {
  for (let i = 0; i < reps; i++) fs.closeSync(fs.openSync(path.join(dir, `${prefix}${i}`), "w"));
};

bench("create+close", each((d, i) => fs.closeSync(fs.openSync(path.join(d, `f${i}`), "w"))));
// Cached: the guest should answer these without crossing the boundary at all.
bench("stat-cached", each((d, i) => fs.statSync(path.join(d, `f${i % 100}`))), touch("f"));
// A name that is not there, which no cache can answer for.
bench(
  "stat-missing",
  each((d, i) => {
    try {
      fs.statSync(path.join(d, `absent${i}`));
    } catch {}
  }),
);
bench("write-4k", each((d, i) => fs.writeFileSync(path.join(d, `w${i}`), Buffer.alloc(4096))));
// The shape a package manager writes in: one file, opened once and filled by a
// decompressor a chunk at a time. Whether those chunks reach the server as one
// request or eight is the whole question a write-back cache exists to answer,
// and a single `writeFileSync` never asks it.
const chunk = Buffer.alloc(8192);
bench(
  "write-chunked",
  each((d, i) => {
    const fd = fs.openSync(path.join(d, `c${i}`), "w");
    for (let n = 0; n < 8; n++) fs.writeSync(fd, chunk);
    fs.closeSync(fd);
  }),
);
bench("unlink", each((d, i) => fs.unlinkSync(path.join(d, `f${i}`))), touch("f"));
// The shape pnpm installs in: every file it imports is hardlinked into the
// virtual store, so an install is as fast as the filesystem's link — and on
// APFS a hardlink is not a cheap operation.
bench(
  "link",
  each((d, i) => fs.linkSync(path.join(d, "source"), path.join(d, `l${i}`))),
  (d) => fs.closeSync(fs.openSync(path.join(d, "source"), "w")),
);
// The other half of an atomic import: write under a temporary name, rename
// into place.
bench(
  "rename",
  each((d, i) => fs.renameSync(path.join(d, `f${i}`), path.join(d, `r${i}`))),
  touch("f"),
);

// The same creates, issued concurrently.
//
// Node's file system calls run on libuv's thread pool, so a batch of promises
// is genuinely several syscalls in flight at once. That matters because it is
// the only case here that can see a lock on either side of the boundary: the
// others measure a path that is never contended, and a change which removes
// contention shows up in them as nothing at all.
async function benchParallel(name) {
  if (only && only !== name) return;
  const samples = new Map(places.map(([suffix]) => [suffix, []]));
  const batches = Math.ceil(reps / PARALLEL);
  for (let round = 0; round < rounds; round++) {
    for (const [suffix, dir] of places) {
      fresh(dir);
      const started = process.hrtime.bigint();
      for (let b = 0; b < batches; b++) {
        const batch = [];
        for (let n = 0; n < PARALLEL; n++) {
          const p = path.join(dir, `p${b * PARALLEL + n}`);
          batch.push(fs.promises.open(p, "w").then((h) => h.close()));
        }
        await Promise.all(batch);
      }
      const took = Number(process.hrtime.bigint() - started) / 1000 / (batches * PARALLEL);
      samples.get(suffix).push(took);
    }
  }
  for (const [suffix] of places) {
    console.log(`US ${name}${suffix} ${median(samples.get(suffix)).toFixed(2)}`);
  }
}

// Called rather than awaited at the top level: the file uses `require`, and
// mixing that with top-level await makes Node refuse to decide which kind of
// module it is.
benchParallel("create-parallel").then(() => {
  for (const [, dir] of places) fs.rmSync(dir, { recursive: true, force: true });
});
