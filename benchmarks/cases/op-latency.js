// Per-operation latency, on whatever filesystem $DIR names.
//
// The suite's other cases are workloads: they run a package manager and report
// how long it took, which is the number anyone cares about and is also the one
// that takes six minutes to produce and moves by two hundred milliseconds
// between runs. This is the instrument rather than the result. It does one
// kind of syscall at a time, several thousand times, and reports microseconds
// — so a change to the transport shows up in fifteen seconds, in the column it
// affected, at a precision the workload cases cannot reach.
//
//   ONLY=create+close   just that one, so a server-side opcode histogram can
//                       be attributed to it rather than to a sum of five
//   OPS=3000            operations per bench
//   PARALLEL=16         batch size for the concurrent bench
const fs = require("node:fs");
const path = require("node:path");

const dir = path.join(process.env.DIR, "op-latency");
const reps = Number(process.env.OPS || 3000);
const only = process.env.ONLY;
const PARALLEL = Number(process.env.PARALLEL || 16);

fs.rmSync(dir, { recursive: true, force: true });
fs.mkdirSync(dir, { recursive: true });

// Each bench makes whatever it needs, untimed. Depending on the one before it
// is fine until `ONLY` runs one on its own and it fails on a file that was
// never created — which is the point of being able to run one on its own.
function bench(name, fn, setup) {
  if (only && only !== name) return;
  if (setup) setup();
  const started = process.hrtime.bigint();
  for (let i = 0; i < reps; i++) fn(i);
  const each = Number(process.hrtime.bigint() - started) / 1000 / reps;
  console.log(`US ${name} ${each.toFixed(2)}`);
}

const touch = (prefix) => () => {
  for (let i = 0; i < reps; i++) {
    const p = path.join(dir, `${prefix}${i}`);
    if (!fs.existsSync(p)) fs.closeSync(fs.openSync(p, "w"));
  }
};

// The same creates, issued concurrently.
//
// Node's file system calls run on libuv's thread pool, so a batch of promises
// is genuinely several syscalls in flight at once. That matters because it is
// the only bench here that can see a lock on the server side: the others
// measure a path that is never contended, and a change which removes
// contention shows up in them as nothing at all.
async function benchParallel(name) {
  if (only && only !== name) return;
  const batches = Math.ceil(reps / PARALLEL);
  const started = process.hrtime.bigint();
  for (let b = 0; b < batches; b++) {
    const batch = [];
    for (let n = 0; n < PARALLEL; n++) {
      const p = path.join(dir, `p${b * PARALLEL + n}`);
      batch.push(fs.promises.open(p, "w").then((h) => h.close()));
    }
    await Promise.all(batch);
  }
  const each = Number(process.hrtime.bigint() - started) / 1000 / (batches * PARALLEL);
  console.log(`US ${name} ${each.toFixed(2)}`);
}

bench("create+close", (i) => fs.closeSync(fs.openSync(path.join(dir, `f${i}`), "w")));
// Cached: the guest should answer these without crossing the boundary at all.
bench("stat-cached", (i) => fs.statSync(path.join(dir, `f${i % 100}`)), touch("f"));
// A name that is not there, which no cache can answer for.
bench("stat-missing", (i) => {
  try {
    fs.statSync(path.join(dir, `absent${i}`));
  } catch {}
});
bench("write-4k", (i) => fs.writeFileSync(path.join(dir, `w${i}`), Buffer.alloc(4096)));
// The shape a package manager writes in: one file, opened once and filled by a
// decompressor a chunk at a time. Whether those chunks reach the server as one
// request or eight is the whole question a write-back cache exists to answer,
// and a single `writeFileSync` never asks it.
const chunk = Buffer.alloc(8192);
bench("write-chunked", (i) => {
  const fd = fs.openSync(path.join(dir, `c${i}`), "w");
  for (let n = 0; n < 8; n++) fs.writeSync(fd, chunk);
  fs.closeSync(fd);
});
bench("unlink", (i) => fs.unlinkSync(path.join(dir, `f${i}`)), touch("f"));

// Called rather than awaited at the top level: the file uses `require`, and
// mixing that with top-level await makes Node refuse to decide which kind of
// module it is.
benchParallel("create-parallel").then(() => {
  fs.rmSync(dir, { recursive: true, force: true });
});
