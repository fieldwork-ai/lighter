// The warm case's containers: a stack that is up all day and never idle.
//
//   writer <dir> <MiB per second> <seconds> [total MiB]
//     Write-once data at a steady rate until the total is written, then the
//     same steady third of a core with nothing more to write: a build cache,
//     a log shipper's morning. Nothing ever reads it back, so every page of
//     it is cache with no reader, and what is left of it at the end is what
//     the policy took in the time it had.
//   reader <label> <dir> <every seconds> <seconds>
//     Reads a whole tree, again and again: a dev server, `git status`, a test
//     run. This is the work a cache policy must not hurt, and each pass
//     prints its wall time.
const fs = require('fs'), path = require('path');
const [mode, ...args] = process.argv.slice(1);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function writer(dir, mibPerSec, seconds, total) {
	fs.mkdirSync(dir, { recursive: true });
	const chunk = Buffer.alloc(1 << 20, 0x5a);
	const end = Date.now() + seconds * 1000;
	let written = 0;
	for (let i = 0; Date.now() < end; i++) {
		const began = Date.now();
		if (written < total) {
			const fd = fs.openSync(path.join(dir, `seg-${i}`), 'w');
			for (let m = 0; m < mibPerSec; m++) { chunk.writeUInt32LE(i * 4096 + m, 0); fs.writeSync(fd, chunk); }
			fs.closeSync(fd);
			written += mibPerSec;
		}
		// A third of each second busy, the writes included.
		while (Date.now() - began < 333) Math.sqrt(Math.random());
		await sleep(Math.max(0, 1000 - (Date.now() - began)));
	}
	console.log(`WROTE ${written} MiB`);
}

function walk(dir) {
	let files = 0, bytes = 0;
	for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
		const full = path.join(dir, entry.name);
		if (entry.isDirectory()) { const sub = walk(full); files += sub.files; bytes += sub.bytes; }
		else if (entry.isFile()) { bytes += fs.readFileSync(full).length; files++; }
	}
	return { files, bytes };
}

async function reader(label, dir, every, seconds) {
	const end = Date.now() + seconds * 1000;
	while (Date.now() < end) {
		const began = process.hrtime.bigint();
		const { files, bytes } = walk(dir);
		const ms = Number((process.hrtime.bigint() - began) / 1000000n);
		console.log(`READ ${label} ${ms} ms ${files} files ${bytes >> 20} MiB`);
		await sleep(every * 1000);
	}
}

const run = mode === 'writer' ? writer(args[0], +args[1], +args[2], args[3] === undefined ? Infinity : +args[3])
	: mode === 'reader' ? reader(args[0], args[1], +args[2], +args[3])
	: Promise.reject(new Error(`unknown mode ${mode}`));
run.catch((e) => { console.error(e.message); process.exit(1); });
