// How long after the host changes a file the guest can see the change.
//
// Measured as a full round trip on one clock — this side writes a request, the
// host answers it, and this side waits for the answer — because the two sides
// have no clock they agree on to milliseconds. That makes the number strictly
// larger than the one being claimed, which is the right direction for a
// benchmark to be wrong in.
//
// A case of its own rather than a shell body because it is the one measurement
// short enough that spawning a shell would dominate it.

const fs = require("node:fs");
const path = require("node:path");

const work = process.env.WORK;
const reps = Number(process.env.REPS || 3);
const request = path.join(work, "request");
const reply = path.join(work, "reply");

const deadlineMs = 30_000;

for (let rep = 1; rep <= reps; rep++) {
  const token = String(rep);
  const started = process.hrtime.bigint();
  fs.writeFileSync(request, token);
  for (;;) {
    let seen = "";
    try {
      seen = fs.readFileSync(reply, "utf8");
    } catch {}
    if (seen === token) break;
    if (Number(process.hrtime.bigint() - started) / 1e6 > deadlineMs) {
      console.error(`round ${rep} was never answered`);
      process.exit(1);
    }
  }
  const elapsed = Number(process.hrtime.bigint() - started) / 1e6;
  console.log(`TIME_MS ${Math.round(elapsed)}`);
}
