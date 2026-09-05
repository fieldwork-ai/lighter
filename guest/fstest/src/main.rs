//! The guest half of the shared-filesystem coherence suite.
//!
//! Everything here is a real syscall against a real mount, because the point is
//! to catch the cases a protocol-level test on the host cannot reach: whether
//! Linux's page cache agrees with what the server thinks it said, whether an
//! `mmap` of a FUSE file works at all, and whether data the guest called
//! `fsync` on is genuinely on the Mac's disk a moment before the VMM is killed.
//!
//! Output is one line per check, `ok` or `FAIL`, with a `FSTEST` prefix so the
//! gate can pick it out of a boot log. The process exits non-zero if anything
//! failed, which is belt and braces: the gate reads the lines, but a prober that
//! died halfway must not look like a clean run.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod mmap;

/// How long a host-coordinated step waits before giving up. Generous: the host
/// side is a shell script, and a gate that fails on a slow laptop is worse than
/// one that takes a few more seconds.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

struct Report {
    failures: usize,
}

impl Report {
    fn check(&mut self, name: &str, outcome: Result<(), String>) {
        match outcome {
            Ok(()) => println!("FSTEST {name} ok"),
            Err(why) => {
                self.failures += 1;
                println!("FSTEST {name} FAIL {why}");
            }
        }
        // The console is a serial port and the gate reads it live; without this
        // a panic further down loses everything already printed.
        let _ = std::io::stdout().flush();
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let dir = PathBuf::from(args.next().unwrap_or_else(|| "/mnt/share".into()));

    let mut report = Report { failures: 0 };
    match mode.as_str() {
        "suite" => suite(&dir, &mut report),
        "cross" => cross(&dir, &mut report),
        "durability" => {
            durability(&dir);
            return;
        }
        other => {
            println!("FSTEST usage FAIL unknown mode {other:?}");
            std::process::exit(2);
        }
    }

    println!("FSTEST complete failures={}", report.failures);
    let _ = std::io::stdout().flush();
    if report.failures > 0 {
        std::process::exit(1);
    }
}

// --- the self-contained suite ----------------------------------------------

fn suite(dir: &Path, report: &mut Report) {
    report.check("mount", mounted(dir));
    report.check("roundtrip", roundtrip(dir));
    report.check("append", append(dir));
    report.check("large-file", large_file(dir));
    report.check("many-files", many_files(dir));
    report.check("unlink-while-open", unlink_while_open(dir));
    report.check("rename-open-directory", rename_open_directory(dir));
    report.check("hard-link", hard_link(dir));
    report.check("symlink", symlink_check(dir));
    report.check("mmap-shared", mmap::shared_mapping(dir));
    report.check("mmap-write-visible", mmap::write_through(dir));
    report.check("fsync-reopen", fsync_reopen(dir));
    report.check("truncate", truncate(dir));
    report.check("sparse-seek", sparse_seek(dir));
    report.check("permissions", permissions(dir));
    report.check("rename-storm", rename_storm(dir));
    report.check("directory-tree", directory_tree(dir));
    report.check("statfs", statfs(dir));
}

fn mounted(dir: &Path) -> Result<(), String> {
    let mounts = fs::read_to_string("/proc/mounts").map_err(|e| e.to_string())?;
    let target = dir.to_string_lossy();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(_), Some(at), Some(kind)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if at == target {
            return if kind == "lighterfs" {
                Ok(())
            } else {
                Err(format!("{target} is a {kind}, not a lighterfs"))
            };
        }
    }
    Err(format!("nothing is mounted at {target}"))
}

fn roundtrip(dir: &Path) -> Result<(), String> {
    let path = dir.join("roundtrip");
    fs::write(&path, b"the quick brown fox").map_err(|e| e.to_string())?;
    let back = fs::read(&path).map_err(|e| e.to_string())?;
    if back != b"the quick brown fox" {
        return Err(format!("read back {} bytes of the wrong thing", back.len()));
    }
    fs::remove_file(&path).map_err(|e| e.to_string())
}

/// `O_APPEND` is the flag whose numbering differs most dangerously between the
/// two systems: Linux's value is macOS's `O_TRUNC | O_EXCL`.
fn append(dir: &Path) -> Result<(), String> {
    let path = dir.join("append");
    fs::write(&path, b"one\n").map_err(|e| e.to_string())?;
    {
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        file.write_all(b"two\n").map_err(|e| e.to_string())?;
    }
    let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    if content != "one\ntwo\n" {
        return Err(format!("expected two lines, got {content:?}"));
    }
    fs::remove_file(&path).map_err(|e| e.to_string())
}

/// Four megabytes in one file, which crosses every buffer boundary the
/// transport has: the negotiated `max_write`, the descriptor chain, and the
/// guest's own readahead window.
fn large_file(dir: &Path) -> Result<(), String> {
    let path = dir.join("large");
    let payload: Vec<u8> = (0..4 << 20).map(|i| (i % 251) as u8).collect();
    fs::write(&path, &payload).map_err(|e| e.to_string())?;
    let back = fs::read(&path).map_err(|e| e.to_string())?;
    if back.len() != payload.len() {
        return Err(format!("{} bytes back, {} written", back.len(), payload.len()));
    }
    if let Some(at) = back.iter().zip(&payload).position(|(a, b)| a != b) {
        return Err(format!("first difference at byte {at}"));
    }
    fs::remove_file(&path).map_err(|e| e.to_string())
}

/// Two thousand files, listed and counted. This is the check that catches a
/// `readdir` that loses one entry per page — which nothing with a handful of
/// files ever notices.
fn many_files(dir: &Path) -> Result<(), String> {
    let nest = dir.join("many");
    let _ = fs::remove_dir_all(&nest);
    fs::create_dir_all(&nest).map_err(|e| e.to_string())?;
    for index in 0..2000 {
        fs::write(nest.join(format!("f{index:04}")), b"x").map_err(|e| e.to_string())?;
    }
    let mut names: Vec<String> = fs::read_dir(&nest)
        .map_err(|e| e.to_string())?
        .map(|entry| {
            entry
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .map_err(|e| e.to_string())
        })
        .collect::<Result<_, _>>()?;
    names.sort();
    if names.len() != 2000 {
        return Err(format!("listed {} of 2000", names.len()));
    }
    if names[0] != "f0000" || names[1999] != "f1999" {
        return Err(format!("wrong first/last: {} .. {}", names[0], names[1999]));
    }
    fs::remove_dir_all(&nest).map_err(|e| e.to_string())
}

/// A file with no remaining links, still open. Compilers and package managers
/// do this constantly, and a server that keyed inodes on paths breaks here.
fn unlink_while_open(dir: &Path) -> Result<(), String> {
    let path = dir.join("doomed");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    file.write_all(b"before").map_err(|e| e.to_string())?;
    fs::remove_file(&path).map_err(|e| e.to_string())?;
    if path.exists() {
        return Err("the name survived the unlink".into());
    }

    file.write_all(b" and after").map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut content = String::new();
    file.read_to_string(&mut content).map_err(|e| e.to_string())?;
    if content != "before and after" {
        return Err(format!("read {content:?} from the unlinked file"));
    }
    Ok(())
}

/// Renaming a directory that has an open file inside it. An inode that is a
/// path rather than a descriptor loses the file here.
fn rename_open_directory(dir: &Path) -> Result<(), String> {
    let from = dir.join("before-move");
    let to = dir.join("after-move");
    let _ = fs::remove_dir_all(&from);
    let _ = fs::remove_dir_all(&to);
    fs::create_dir_all(&from).map_err(|e| e.to_string())?;
    fs::write(from.join("held"), b"payload").map_err(|e| e.to_string())?;

    let mut file = File::open(from.join("held")).map_err(|e| e.to_string())?;
    fs::rename(&from, &to).map_err(|e| e.to_string())?;

    let mut content = String::new();
    file.read_to_string(&mut content).map_err(|e| e.to_string())?;
    if content != "payload" {
        return Err(format!("open handle now reads {content:?}"));
    }
    if !to.join("held").exists() {
        return Err("the file did not move with its directory".into());
    }
    fs::remove_dir_all(&to).map_err(|e| e.to_string())
}

fn hard_link(dir: &Path) -> Result<(), String> {
    let first = dir.join("link-a");
    let second = dir.join("link-b");
    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
    fs::write(&first, b"shared").map_err(|e| e.to_string())?;
    fs::hard_link(&first, &second).map_err(|e| e.to_string())?;

    let a = fs::metadata(&first).map_err(|e| e.to_string())?;
    let b = fs::metadata(&second).map_err(|e| e.to_string())?;
    if a.ino() != b.ino() {
        return Err(format!("inodes differ: {} and {}", a.ino(), b.ino()));
    }
    if a.nlink() != 2 {
        return Err(format!("link count is {}, expected 2", a.nlink()));
    }
    fs::remove_file(&second).map_err(|e| e.to_string())?;
    if fs::metadata(&first).map_err(|e| e.to_string())?.nlink() != 1 {
        return Err("link count did not fall when one name was removed".into());
    }
    fs::remove_file(&first).map_err(|e| e.to_string())
}

fn symlink_check(dir: &Path) -> Result<(), String> {
    let target = dir.join("symlink-target");
    let link = dir.join("symlink");
    let _ = fs::remove_file(&link);
    fs::write(&target, b"pointed at").map_err(|e| e.to_string())?;
    symlink("symlink-target", &link).map_err(|e| e.to_string())?;

    let read = fs::read_link(&link).map_err(|e| e.to_string())?;
    if read != Path::new("symlink-target") {
        return Err(format!("link points at {read:?}"));
    }
    // Following it is the other half: a `readlink` that returned a trailing
    // NUL would produce a path that does not resolve.
    let through = fs::read(&link).map_err(|e| e.to_string())?;
    if through != b"pointed at" {
        return Err("reading through the link gave the wrong contents".into());
    }
    if !fs::symlink_metadata(&link)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("lstat does not report a symlink".into());
    }
    fs::remove_file(&link).map_err(|e| e.to_string())?;
    fs::remove_file(&target).map_err(|e| e.to_string())
}

fn fsync_reopen(dir: &Path) -> Result<(), String> {
    let path = dir.join("synced");
    let mut file = File::create(&path).map_err(|e| e.to_string())?;
    file.write_all(b"durable bytes").map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);

    let back = fs::read(&path).map_err(|e| e.to_string())?;
    if back != b"durable bytes" {
        return Err("contents changed across a close and reopen".into());
    }
    fs::remove_file(&path).map_err(|e| e.to_string())
}

fn truncate(dir: &Path) -> Result<(), String> {
    let path = dir.join("truncated");
    fs::write(&path, vec![b'x'; 8192]).map_err(|e| e.to_string())?;

    let file = OpenOptions::new()
        .write(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    file.set_len(100).map_err(|e| e.to_string())?;
    if fs::metadata(&path).map_err(|e| e.to_string())?.len() != 100 {
        return Err("shrinking did not take".into());
    }

    // Extending must produce zeroes, not whatever was there before.
    file.set_len(4096).map_err(|e| e.to_string())?;
    let back = fs::read(&path).map_err(|e| e.to_string())?;
    if back.len() != 4096 {
        return Err(format!("extended to {} bytes", back.len()));
    }
    if back[100..].iter().any(|&b| b != 0) {
        return Err("the extension is not zero-filled".into());
    }
    fs::remove_file(&path).map_err(|e| e.to_string())
}

/// `SEEK_DATA` and `SEEK_HOLE` are numbered the opposite way round on the two
/// systems, so a server that forwards the constant answers the other question.
fn sparse_seek(dir: &Path) -> Result<(), String> {
    let path = dir.join("sparse");
    let mut file = File::create(&path).map_err(|e| e.to_string())?;
    file.set_len(1 << 20).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start((1 << 20) - 4))
        .map_err(|e| e.to_string())?;
    file.write_all(b"tail").map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;

    // Asking for the next data from offset 0 must not hand back the end of the
    // file, which is what "the next hole" would be.
    let data = file.seek(SeekFrom::Start(0)).map(|_| ()).and_then(|_| {
        // SEEK_DATA is 3 on Linux. std has no wrapper, so this is the raw call.
        let at = unsafe { libc::lseek(std::os::fd::AsRawFd::as_raw_fd(&file), 0, 3) };
        if at < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(at as u64)
        }
    });
    match data {
        // A filesystem with no sparse support answers 0, which is also correct.
        Ok(at) if at <= 1 << 20 => {}
        Ok(at) => return Err(format!("SEEK_DATA landed past the end at {at}")),
        // ENXIO means "no data after here", which cannot be true: we wrote some.
        Err(e) => return Err(format!("SEEK_DATA failed: {e}")),
    }
    fs::remove_file(&path).map_err(|e| e.to_string())
}

fn permissions(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("modes");
    fs::write(&path, b"x").map_err(|e| e.to_string())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let mode = fs::metadata(&path).map_err(|e| e.to_string())?.mode() & 0o777;
    if mode != 0o600 {
        return Err(format!("mode is {mode:o} after chmod 600"));
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    let mode = fs::metadata(&path).map_err(|e| e.to_string())?.mode() & 0o777;
    if mode != 0o755 {
        return Err(format!("mode is {mode:o} after chmod 755"));
    }
    fs::remove_file(&path).map_err(|e| e.to_string())
}

/// A thousand renames of one file, with a reader chasing it. A build tool
/// writing atomically does exactly this, and the failure mode is a stale inode
/// that answers with the wrong contents rather than an error.
fn rename_storm(dir: &Path) -> Result<(), String> {
    let nest = dir.join("storm");
    let _ = fs::remove_dir_all(&nest);
    fs::create_dir_all(&nest).map_err(|e| e.to_string())?;

    let mut current = nest.join("step-0");
    fs::write(&current, b"carried through").map_err(|e| e.to_string())?;
    let held = File::open(&current).map_err(|e| e.to_string())?;

    for step in 1..1000 {
        let next = nest.join(format!("step-{step}"));
        fs::rename(&current, &next).map_err(|e| format!("rename {step}: {e}"))?;
        current = next;
    }

    // The descriptor opened before the first rename must still be the file.
    let mut content = String::new();
    (&held)
        .read_to_string(&mut content)
        .map_err(|e| e.to_string())?;
    if content != "carried through" {
        return Err(format!("the held descriptor reads {content:?}"));
    }
    let final_content = fs::read(&current).map_err(|e| e.to_string())?;
    if final_content != b"carried through" {
        return Err("the final name has the wrong contents".into());
    }
    let listed = fs::read_dir(&nest).map_err(|e| e.to_string())?.count();
    if listed != 1 {
        return Err(format!("{listed} files left behind by 999 renames"));
    }
    fs::remove_dir_all(&nest).map_err(|e| e.to_string())
}

/// Depth, which exercises the inode table rather than any one operation: every
/// level is a lookup, and a server that leaked or dropped references shows it
/// here first.
fn directory_tree(dir: &Path) -> Result<(), String> {
    let root = dir.join("tree");
    let _ = fs::remove_dir_all(&root);
    let mut deep = root.clone();
    for level in 0..40 {
        deep = deep.join(format!("level-{level}"));
    }
    fs::create_dir_all(&deep).map_err(|e| e.to_string())?;
    fs::write(deep.join("leaf"), b"bottom").map_err(|e| e.to_string())?;
    if fs::read(deep.join("leaf")).map_err(|e| e.to_string())? != b"bottom" {
        return Err("the leaf did not survive the walk".into());
    }
    fs::remove_dir_all(&root).map_err(|e| e.to_string())
}

fn statfs(dir: &Path) -> Result<(), String> {
    let path = std::ffi::CString::new(dir.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut buf) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if buf.f_blocks == 0 {
        return Err("the filesystem reports no blocks at all".into());
    }
    if buf.f_namelen != 255 {
        return Err(format!("NAME_MAX reported as {}", buf.f_namelen));
    }
    Ok(())
}

// --- host-coordinated coherence --------------------------------------------

/// Waits for the host to leave a marker, then returns.
fn await_marker(dir: &Path, name: &str) -> Result<(), String> {
    let marker = dir.join(name);
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    while Instant::now() < deadline {
        if marker.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("the host never left {name}"))
}

fn mark(dir: &Path, name: &str) -> Result<(), String> {
    let path = dir.join(name);
    let file = File::create(&path).map_err(|e| e.to_string())?;
    // Synced, because the host is about to poll for it and an entry sitting in
    // a cache is a handshake that never completes.
    file.sync_all().map_err(|e| e.to_string())
}

/// The half of the suite that needs someone on the other side.
///
/// Each step is a full round trip: the guest does something, tells the host, and
/// waits to be told the host saw it. That is what makes it a coherence test
/// rather than two independent filesystems that happen to agree.
fn cross(dir: &Path, report: &mut Report) {
    report.check("cross-mount", mounted(dir));

    report.check(
        "guest-write-visible-to-host",
        (|| {
            let path = dir.join("guest-wrote");
            let mut file = File::create(&path).map_err(|e| e.to_string())?;
            file.write_all(b"written inside the guest")
                .map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            drop(file);
            mark(dir, "guest-wrote.done")?;
            await_marker(dir, "host-saw-guest-write")
        })(),
    );

    report.check(
        "host-write-visible-to-guest",
        (|| {
            await_marker(dir, "host-wrote.done")?;
            let content = fs::read(dir.join("host-wrote")).map_err(|e| e.to_string())?;
            if content != b"written on the host" {
                return Err(format!(
                    "the guest sees {:?}",
                    String::from_utf8_lossy(&content)
                ));
            }
            Ok(())
        })(),
    );

    report.check(
        "host-overwrite-is-seen",
        (|| {
            // The same path, changed on the host after the guest has already
            // read it once. A cached attribute or a stale page here is the
            // classic bind-mount complaint.
            await_marker(dir, "host-overwrote.done")?;
            let content = fs::read(dir.join("host-wrote")).map_err(|e| e.to_string())?;
            if content != b"changed on the host" {
                return Err(format!(
                    "the guest still sees {:?}",
                    String::from_utf8_lossy(&content)
                ));
            }
            Ok(())
        })(),
    );

    report.check(
        "host-rename-is-seen",
        (|| {
            await_marker(dir, "host-renamed.done")?;
            if dir.join("host-wrote").exists() {
                return Err("the old name is still visible".into());
            }
            let content = fs::read(dir.join("host-renamed")).map_err(|e| e.to_string())?;
            if content != b"changed on the host" {
                return Err("the renamed file has the wrong contents".into());
            }
            Ok(())
        })(),
    );

    report.check(
        "host-delete-is-seen",
        (|| {
            await_marker(dir, "host-deleted.done")?;
            if dir.join("host-renamed").exists() {
                return Err("a file deleted on the host is still listed".into());
            }
            Ok(())
        })(),
    );

    report.check(
        "new-host-directory-is-listed",
        (|| {
            await_marker(dir, "host-tree.done")?;
            let mut names: Vec<String> = fs::read_dir(dir.join("host-tree"))
                .map_err(|e| e.to_string())?
                .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect();
            names.sort();
            if names != ["one", "three", "two"] {
                return Err(format!("listed {names:?}"));
            }
            Ok(())
        })(),
    );

    let _ = mark(dir, "guest-finished");
}

// --- durability -------------------------------------------------------------

/// Writes, syncs, announces, and then keeps the machine busy forever.
///
/// The gate kills the VMM with `SIGKILL` once it sees the announcement, so
/// nothing after this point gets a chance to flush anything. Whatever is in the
/// host file afterwards is what `fsync` actually guaranteed.
fn durability(dir: &Path) {
    let payload: Vec<u8> = (0..1 << 20).map(|i| (i % 241) as u8).collect();
    let path = dir.join("durable");
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            // O_DSYNC on top of the explicit fsync below, because the two
            // failure modes are different: one tests the guest's flush path,
            // the other the server's.
            .custom_flags(libc::O_DSYNC)
            .open(&path)?;
        file.write_all(&payload)?;
        file.sync_all()
    })();

    match result {
        Ok(()) => println!("FSTEST durable-synced bytes={}", payload.len()),
        Err(e) => println!("FSTEST durable-synced FAIL {e}"),
    }
    let _ = std::io::stdout().flush();

    // Keep writing somewhere else so the machine is unambiguously alive and
    // mid-flight when it is killed. Nothing checks this file.
    let scratch = dir.join("scratch");
    loop {
        let _ = fs::write(&scratch, b"still running");
        std::thread::sleep(Duration::from_millis(20));
    }
}
