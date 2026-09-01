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

// `ONLY=create+close` narrows it to one operation, which is what attributing
// a request count to a syscall needs: with five benches running, a server-side
// opcode histogram is a sum and says nothing about which one produced what.
const only = process.env.ONLY;

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
bench("unlink", (i) => fs.unlinkSync(path.join(dir, `f${i}`)), touch("f"));

fs.rmSync(dir, { recursive: true, force: true });
