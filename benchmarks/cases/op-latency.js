// Per-operation latency, single-threaded, on whatever filesystem $DIR names.
//
// The suite's other cases are workloads: they run a package manager and report
// how long it took, which is the number anyone actually cares about and is
// also the number that takes six minutes to produce and moves by a second
// between runs. This one is the instrument rather than the result. It does one
// syscall at a time, several thousand times, and reports microseconds — so a
// change to the transport shows up in ninety seconds, in the column it
// affected, at a precision the workload cases cannot reach.
//
// Single-threaded on purpose. Concurrency hides latency, and hiding it is what
// makes a workload number hard to attribute.
const fs = require("node:fs");
const path = require("node:path");

const dir = path.join(process.env.DIR, "op-latency");
const reps = Number(process.env.OPS || 3000);
fs.rmSync(dir, { recursive: true, force: true });
fs.mkdirSync(dir, { recursive: true });

function bench(name, fn) {
  const started = process.hrtime.bigint();
  for (let i = 0; i < reps; i++) fn(i);
  const each = Number(process.hrtime.bigint() - started) / 1000 / reps;
  console.log(`US ${name} ${each.toFixed(2)}`);
}

bench("create+close", (i) => fs.closeSync(fs.openSync(path.join(dir, `f${i}`), "w")));
// Cached: the guest should answer these without crossing the boundary at all.
bench("stat-cached", (i) => fs.statSync(path.join(dir, `f${i % 100}`)));
// A name that is not there, which no cache can answer for.
bench("stat-missing", (i) => {
  try {
    fs.statSync(path.join(dir, `absent${i}`));
  } catch {}
});
bench("write-4k", (i) => fs.writeFileSync(path.join(dir, `w${i}`), Buffer.alloc(4096)));
bench("unlink", (i) => fs.unlinkSync(path.join(dir, `f${i}`)));

fs.rmSync(dir, { recursive: true, force: true });
