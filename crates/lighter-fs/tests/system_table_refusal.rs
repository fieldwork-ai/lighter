//! The Mac's own file table is full, and a local share meets it on an open.
//!
//! Its own binary, because the table belongs to the whole machine and cannot
//! be filled from a test without failing everything else on it. The refusal
//! is injected instead, through `lighter_fs::sys::refuse_next_opens`, at the
//! same point a full table would refuse the open. The seam is process-wide,
//! so the tests here take turns.

use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use lighter_fs::fuse::{self, op};
use lighter_fs::inode::Pool;
use lighter_fs::{FillError, Server, Sink, SinkFull};

/// Linux numbers, as the guest reads them. The same on the Mac for these two.
const ENFILE: i32 = 23;
const EMFILE: i32 = 24;

static TURNS: Mutex<()> = Mutex::new(());

fn turn() -> MutexGuard<'static, ()> {
    TURNS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Buffer(Vec<u8>);

impl Sink for Buffer {
    fn capacity(&self) -> usize {
        (1 << 20) - self.0.len()
    }

    fn write(&mut self, data: &[u8]) -> Result<(), SinkFull> {
        self.0.extend_from_slice(data);
        Ok(())
    }

    fn fill(&mut self, fd: RawFd, offset: u64, len: usize) -> Result<usize, FillError> {
        let start = self.0.len();
        self.0.resize(start + len, 0);
        match lighter_fs::sys::read_at(fd, &mut self.0[start..], offset) {
            Ok(read) => {
                self.0.truncate(start + read);
                Ok(read)
            }
            Err(errno) => {
                self.0.truncate(start);
                Err(FillError::Read(errno))
            }
        }
    }

    fn rewrite_head(&mut self, head: &[u8]) -> Result<(), SinkFull> {
        self.0[..head.len()].copy_from_slice(head);
        Ok(())
    }
}

struct Guest {
    server: Server,
    unique: u64,
}

impl Guest {
    fn call(&mut self, opcode: u32, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        self.unique += 1;
        let mut request = Vec::new();
        request.extend_from_slice(&((fuse::IN_HEADER_LEN + body.len()) as u32).to_le_bytes());
        request.extend_from_slice(&opcode.to_le_bytes());
        request.extend_from_slice(&self.unique.to_le_bytes());
        request.extend_from_slice(&nodeid.to_le_bytes());
        request.extend_from_slice(&[0u8; 16]); // uid, gid, pid, total_extlen
        request.extend_from_slice(body);
        let mut reply = Buffer(Vec::new());
        if self.server.dispatch(&request, &mut reply) == 0 {
            return Ok(Vec::new());
        }
        let error = i32::from_le_bytes(reply.0[4..8].try_into().unwrap());
        if error != 0 {
            return Err(-error);
        }
        Ok(reply.0[fuse::OUT_HEADER_LEN..].to_vec())
    }

    fn lookup(&mut self, parent: u64, name: &str) -> Result<u64, i32> {
        let mut body = name.as_bytes().to_vec();
        body.push(0);
        let reply = self.call(op::LOOKUP, parent, &body)?;
        Ok(u64::from_le_bytes(reply[0..8].try_into().unwrap()))
    }

    fn create(&mut self, parent: u64, name: &str) -> Result<u64, i32> {
        let mut body = Vec::new();
        body.extend_from_slice(&0o101u32.to_le_bytes()); // O_WRONLY | O_CREAT
        body.extend_from_slice(&0o644u32.to_le_bytes()); // mode
        body.extend_from_slice(&0u32.to_le_bytes()); // umask
        body.extend_from_slice(&0u32.to_le_bytes()); // open flags
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        let reply = self.call(op::CREATE, parent, &body)?;
        Ok(u64::from_le_bytes(reply[0..8].try_into().unwrap()))
    }
}

/// A local share over a root of `dirs` directories, each holding one file,
/// with the share's descriptor budget set to `budget`. The root is looked up
/// by the guest's `INIT`, as a mount's is, and nothing else is opened yet.
fn share(name: &str, dirs: usize, budget: &str) -> (Guest, Pool, PathBuf) {
    let root =
        std::env::temp_dir().join(format!("lighter-fs-system-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for d in 0..dirs {
        let dir = root.join(format!("d{d}"));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("f"), format!("{d}")).unwrap();
    }
    // SAFETY: single-threaded test setup, under `turn()`, before the server
    // reads either variable.
    unsafe { std::env::set_var("LIGHTER_FS_FD_BUDGET", budget) };
    let pool = Pool::default();
    let server = Server::new_in_pool(&root, &pool).unwrap();
    let mut guest = Guest { server, unique: 1 };
    let mut init = Vec::new();
    init.extend_from_slice(&7u32.to_le_bytes());
    init.extend_from_slice(&38u32.to_le_bytes());
    init.extend_from_slice(&(128u32 * 1024).to_le_bytes());
    init.extend_from_slice(&u32::MAX.to_le_bytes());
    init.resize(64, 0);
    guest.call(op::INIT, 0, &init).unwrap();
    (guest, pool, root)
}

/// Puts back what `share` and the seam changed, and removes the tree.
fn finish(root: &Path) {
    lighter_fs::sys::refuse_next_opens(0, 0);
    // SAFETY: under `turn()`, after the server is done with the variables.
    unsafe {
        std::env::remove_var("LIGHTER_FS_FD_BUDGET");
        std::env::remove_var("LIGHTER_FS_ASYNC");
        std::env::remove_var("LIGHTER_FS_DEFER_CREATE");
    }
    let _ = std::fs::remove_dir_all(root);
}

/// The Mac's table is full: the open of a name the share has not seen fails
/// with `ENFILE` once. The share lets go of what it holds, halves its budget,
/// and serves the lookup again, which succeeds. A second refusal inside the
/// window is served the same way, and does not halve the budget again.
#[test]
fn a_full_table_is_let_go_of_and_a_lookup_is_asked_again() {
    let _turn = turn();
    let (mut guest, pool, root) = share("lookup", 302, "4096");
    for d in 0..300 {
        guest.lookup(1, &format!("d{d}")).expect("the walk");
    }
    let (held, budget) = guest.server.descriptor_usage();
    assert_eq!(budget, 4096);
    assert!(held >= 300, "the walk held only {held} descriptors");

    lighter_fs::sys::refuse_next_opens(ENFILE, 1);
    let found = guest.lookup(1, "d300");
    let (held_after, budget_after) = guest.server.descriptor_usage();
    let pooled = pool.descriptors();

    lighter_fs::sys::refuse_next_opens(ENFILE, 1);
    let again = guest.lookup(1, "d301");
    let budget_again = guest.server.descriptor_usage().1;
    finish(&root);

    assert!(
        found.is_ok(),
        "the refused lookup was not asked again: {found:?}"
    );
    assert_eq!(budget_after, 2048, "the budget was not halved");
    assert!(
        held_after < held / 2,
        "held {held_after} descriptors after the relief, {held} before"
    );
    assert_eq!(pooled, held_after, "the relief did not count into the pool");
    assert!(
        again.is_ok(),
        "a second refusal was not asked again: {again:?}"
    );
    assert_eq!(budget_again, 2048, "a second relief inside the window");
}

/// A parked file is reopened when it is opened, and a full table refusing
/// that reopen is passed on as it is. The share lets go of what it holds,
/// which it would not do if the refusal were read as the file having gone,
/// and the open is answered with the refusal, as a mutating request is.
#[test]
fn a_full_table_refusing_a_reopen_is_passed_on_and_let_go_of() {
    let _turn = turn();
    let (mut guest, _pool, root) = share("reopen", 301, "4096");
    let dir = guest.lookup(1, "d0").expect("the walk");
    let file = guest.lookup(dir, "f").expect("the walk");
    for d in 1..300 {
        guest.lookup(1, &format!("d{d}")).expect("the walk");
    }
    let (held, _) = guest.server.descriptor_usage();
    assert!(held >= 300, "the walk held only {held} descriptors");

    // The lookup's open is refused once: the share lets go, which parks the
    // walked directories, and the lookup is asked again.
    lighter_fs::sys::refuse_next_opens(ENFILE, 1);
    let found = guest.lookup(1, "d300");
    let (held_after, budget_after) = guest.server.descriptor_usage();
    assert!(
        found.is_ok(),
        "the refused lookup was not asked again: {found:?}"
    );
    assert_eq!(budget_after, 2048, "the budget was not halved");
    assert!(
        held_after < held / 2,
        "held {held_after} after the relief, {held} before"
    );

    // A relief is throttled to one a second, so the next one needs the window
    // to pass. A seek needs the file's descriptor, so it opens the parked file
    // again, and the refusal of that open is the one that lets go.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    lighter_fs::sys::refuse_next_opens(ENFILE, 1);
    let sought = guest.call(op::LSEEK, file, &[0u8; 24]);
    let budget_again = guest.server.descriptor_usage().1;
    finish(&root);

    assert!(
        sought.is_ok(),
        "the refused reopen was not asked again: {sought:?}"
    );
    assert_eq!(
        budget_again, 1024,
        "the reopen's refusal did not let the table go"
    );
}

/// A create on a full table is mutating, so the refusal is answered as it
/// is, once, and nothing is asked again. The table is let go of all the same,
/// and the next attempt, against a table with room, succeeds.
///
/// The apply queue is off for this test, so the create is made in the
/// request and its refusal reaches the guest, instead of being acknowledged
/// and failing later in the queue.
#[test]
fn a_create_refused_by_a_full_table_is_answered_once_and_the_next_succeeds() {
    let _turn = turn();
    // SAFETY: under `turn()`, before the server is built.
    unsafe { std::env::set_var("LIGHTER_FS_ASYNC", "0") };
    let (mut guest, _pool, root) = share("create", 1, "4096");

    lighter_fs::sys::refuse_next_opens(ENFILE, 1);
    let first = guest.create(1, "made");
    let budget = guest.server.descriptor_usage().1;
    let made_by_the_first = root.join("made").exists();
    let second = guest.create(1, "made");
    let made = root.join("made").exists();
    finish(&root);

    assert_eq!(first, Err(ENFILE), "the refusal was not answered as it is");
    assert!(!made_by_the_first, "a refused create left a file behind");
    assert_eq!(budget, 2048, "the table was not let go of");
    assert!(second.is_ok(), "the next create failed: {second:?}");
    assert!(made, "the next create did not make the file");
}

/// `EMFILE` is this process's own limit, which the budget already keeps the
/// share under. It is not the table being full, so nothing is let go of and
/// the budget does not move.
#[test]
fn a_per_process_limit_is_not_the_full_table_and_relieves_nothing() {
    let _turn = turn();
    let (mut guest, _pool, root) = share("emfile", 51, "4096");
    for d in 0..50 {
        guest.lookup(1, &format!("d{d}")).expect("the walk");
    }
    let (held, _) = guest.server.descriptor_usage();

    lighter_fs::sys::refuse_next_opens(EMFILE, 1);
    let refused = guest.lookup(1, "d50");
    let (held_after, budget_after) = guest.server.descriptor_usage();
    finish(&root);

    assert!(
        refused.is_err(),
        "the refusal was not passed on: {refused:?}"
    );
    assert_eq!(budget_after, 4096, "EMFILE lowered the budget");
    assert!(
        held_after >= held,
        "EMFILE let go of {} descriptors",
        held - held_after
    );
}

/// The create is acknowledged before it is made, so the refusal of its open
/// reaches no request: it is found in the apply queue, the way the incident
/// log found it ("an acknowledged create failed to apply", errno 23). The
/// queue's job has to let the table go, or a storm of creates fails job after
/// job with nothing relieving it.
#[test]
fn an_acknowledged_create_refused_by_a_full_table_relieves_it_from_the_queue() {
    let _turn = turn();
    // Straight into the queue: a create held back for its grace period would
    // be answered from the held entry, and its job would not have run.
    // SAFETY: under `turn()`, before the server is built.
    unsafe { std::env::set_var("LIGHTER_FS_DEFER_CREATE", "0") };
    let (mut guest, _pool, root) = share("queued", 1, "4096");

    lighter_fs::sys::refuse_next_opens(ENFILE, 1);
    let acknowledged = guest.create(1, "queued");
    // The job runs on the queue's worker after the request was answered, so
    // wait for the relief it should make, with a deadline for when it does not.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while guest.server.descriptor_usage().1 == 4096 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let budget = guest.server.descriptor_usage().1;
    let made = root.join("queued").exists();
    finish(&root);

    assert!(
        acknowledged.is_ok(),
        "the create was not acknowledged: {acknowledged:?}"
    );
    assert_eq!(budget, 2048, "the queue's refusal did not let the table go");
    assert!(!made, "the refused create made the file anyway");
}
