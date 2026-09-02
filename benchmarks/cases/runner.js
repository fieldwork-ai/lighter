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
// A case that runs this far past any plausible result is a finding, not a
// measurement: a sixteen-minute tree walk once sat in the suite where a
// one-second one belonged, and the only thing the extra fifteen minutes
// bought was the wait. Killed, recorded as a timeout, and the suite goes on.
const limitS = Number(process.env.CASE_TIMEOUT_S || 300);

function sh(script, limitMs) {
  // Errors are inherited rather than swallowed: a case that fails should say
  // why, in the harness output, rather than becoming a missing row.
  // Both streams, not just stderr: pnpm reports its failures through its
  // reporter, which writes to stdout. The TIME_MS lines are extracted by
  // pattern, so case chatter on stdout costs nothing.
  execFileSync("/bin/sh", [script], {
    stdio: ["ignore", "inherit", "inherit"],
    env: process.env,
    timeout: limitMs,
    killSignal: "SIGKILL",
  });
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
  try {
    sh(body, limitS * 1000);
  } catch (err) {
    if (err.code === "ETIMEDOUT" || err.signal === "SIGKILL") {
      console.log(`TIME_MS TIMEOUT ${limitS}s`);
      continue;
    }
    throw err;
  }
  const elapsed = Number(process.hrtime.bigint() - started) / 1e6;
  console.log(`TIME_MS ${Math.round(elapsed)}`);
  // What the children spent, not just how long they took: the same install
  // on two runtimes with equal CPU time but different wall time was waiting,
  // and with different CPU time was running slower. Linux only — the fields
  // are /proc's, and only the guest targets have a kernel worth asking.
  const cpu = childCpuMs();
  if (cpu) console.log(`CPU_MS user=${cpu.user} sys=${cpu.sys}`);
}

function childCpuMs() {
  let stat;
  try {
    stat = fs.readFileSync("/proc/self/stat", "utf8");
  } catch {
    return null;
  }
  // Fields after the parenthesised command name; cutime and cstime are the
  // 16th and 17th of the whole line, in clock ticks (100 Hz on every
  // architecture that matters here).
  const fields = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
  const cutime = Number(fields[13]);
  const cstime = Number(fields[14]);
  const user = cutime * 10 - (childCpuMs.user || 0);
  const sys = cstime * 10 - (childCpuMs.sys || 0);
  childCpuMs.user = cutime * 10;
  childCpuMs.sys = cstime * 10;
  return { user, sys };
}
