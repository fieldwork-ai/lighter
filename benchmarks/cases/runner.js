// Times a case, from inside whatever is running it.
//
// The harness used to time each repetition from the host, around a whole
// `docker run`. That measured container startup: a metadata walk costing the
// filesystem 1,566 requests reported 550ms, of which about 450ms was Docker
// creating and destroying a container. The native target pays no such cost, so
// the comparison was not between two filesystems at all.
//
// So the loop lives inside the target now, and everything before the first
// measurement — image pull, container start, a cold page cache — happens once
// and is not timed. Node does the timing because it is the one runtime every
// target already needs, `date` has no sub-second form on macOS, and spawning a
// clock process per measurement would cost more than several of the cases.

const { execFileSync } = require("node:child_process");
const fs = require("node:fs");

const work = process.env.WORK;
const name = process.argv[2];
const reps = Number(process.env.REPS || 3);

function sh(script) {
  // Errors are inherited rather than swallowed: a case that fails should say
  // why, in the harness output, rather than becoming a missing row.
  // Both streams, not just stderr: pnpm reports its failures through its
  // reporter, which writes to stdout. The TIME_MS lines are extracted by
  // pattern, so case chatter on stdout costs nothing.
  execFileSync("/bin/sh", [script], { stdio: ["ignore", "inherit", "inherit"], env: process.env });
}

const setup = `${work}/cases/${name}.setup.sh`;
const body = `${work}/cases/${name}.sh`;

for (let rep = 0; rep < reps; rep++) {
  if (fs.existsSync(setup)) {
    // A failing setup is not a failing measurement: most of them are a `rm -rf`
    // of something that may not be there yet.
    try {
      sh(setup);
    } catch {}
  }
  const started = process.hrtime.bigint();
  sh(body);
  const elapsed = Number(process.hrtime.bigint() - started) / 1e6;
  console.log(`TIME_MS ${Math.round(elapsed)}`);
}
