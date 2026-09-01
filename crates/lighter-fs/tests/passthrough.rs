//! The filesystem, exercised without a VM.
//!
//! Every one of these drives the server through the same entry point the
//! virtio-fs device uses, with byte-for-byte the requests a Linux guest sends.
//! That is the point: protocol bugs found here cost seconds, and the same bugs
//! found through a booted guest cost an afternoon of reading kernel traces to
//! discover that a structure was two fields long.

use std::path::{Path, PathBuf};

use lighter_fs::fuse::{self, op};
use lighter_fs::{Server, Sink, SinkFull};

/// A reply buffer of a fixed size, so the capacity logic is exercised rather
/// than assumed away.
struct Buffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl Buffer {
    fn new(limit: usize) -> Buffer {
        Buffer {
            bytes: Vec::new(),
            limit,
        }
    }
}

impl Sink for Buffer {
    fn capacity(&self) -> usize {
        self.limit - self.bytes.len()
    }

    fn write(&mut self, data: &[u8]) -> Result<(), SinkFull> {
        if data.len() > self.capacity() {
            return Err(SinkFull);
        }
        self.bytes.extend_from_slice(data);
        Ok(())
    }
}

struct Guest {
    server: Server,
    unique: u64,
    root: PathBuf,
}

impl Guest {
    fn new(name: &str) -> Guest {
        let root = std::env::temp_dir().join(format!(
            "lighter-fs-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let server = Server::new(&root).unwrap();
        let mut guest = Guest {
            server,
            unique: 1,
            root,
        };
        // Every real guest opens with INIT, and several replies depend on what
        // was negotiated there.
        let mut init = Vec::new();
        init.extend_from_slice(&7u32.to_le_bytes());
        init.extend_from_slice(&38u32.to_le_bytes());
        init.extend_from_slice(&(128u32 * 1024).to_le_bytes());
        init.extend_from_slice(&u32::MAX.to_le_bytes());
        init.resize(64, 0);
        guest.call(op::INIT, 0, &init).expect("INIT must succeed");
        guest
    }

    fn call(&mut self, opcode: u32, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, i32> {
        self.call_with(opcode, nodeid, body, 1 << 20)
    }

    fn call_with(
        &mut self,
        opcode: u32,
        nodeid: u64,
        body: &[u8],
        limit: usize,
    ) -> Result<Vec<u8>, i32> {
        self.unique += 1;
        let mut request = Vec::with_capacity(fuse::IN_HEADER_LEN + body.len());
        request.extend_from_slice(&((fuse::IN_HEADER_LEN + body.len()) as u32).to_le_bytes());
        request.extend_from_slice(&opcode.to_le_bytes());
        request.extend_from_slice(&self.unique.to_le_bytes());
        request.extend_from_slice(&nodeid.to_le_bytes());
        request.extend_from_slice(&0u32.to_le_bytes()); // uid: the guest is root
        request.extend_from_slice(&0u32.to_le_bytes()); // gid
        request.extend_from_slice(&1u32.to_le_bytes()); // pid
        request.extend_from_slice(&0u32.to_le_bytes()); // total_extlen
        request.extend_from_slice(body);

        let mut buffer = Buffer::new(limit);
        let written = self.server.dispatch(&request, &mut buffer);
        if written == 0 {
            return Ok(Vec::new());
        }
        let reply = buffer.bytes;
        let len = u32::from_le_bytes(reply[0..4].try_into().unwrap()) as usize;
        let error = i32::from_le_bytes(reply[4..8].try_into().unwrap());
        let unique = u64::from_le_bytes(reply[8..16].try_into().unwrap());
        assert_eq!(
            unique, self.unique,
            "reply must name the request it answers"
        );
        assert_eq!(
            len,
            reply.len(),
            "header length must match what was written"
        );
        if error != 0 {
            return Err(-error);
        }
        Ok(reply[fuse::OUT_HEADER_LEN..].to_vec())
    }

    fn lookup(&mut self, parent: u64, name: &str) -> Result<u64, i32> {
        let mut body = name.as_bytes().to_vec();
        body.push(0);
        let reply = self.call(op::LOOKUP, parent, &body)?;
        Ok(u64::from_le_bytes(reply[0..8].try_into().unwrap()))
    }

    /// CREATE, returning `(nodeid, fh)`.
    fn create(&mut self, parent: u64, name: &str, flags: u32) -> Result<(u64, u64), i32> {
        let mut body = Vec::new();
        body.extend_from_slice(&flags.to_le_bytes());
        body.extend_from_slice(&0o644u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // umask
        body.extend_from_slice(&0u32.to_le_bytes()); // open_flags
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        let reply = self.call(op::CREATE, parent, &body)?;
        let nodeid = u64::from_le_bytes(reply[0..8].try_into().unwrap());
        let fh = u64::from_le_bytes(
            reply[fuse::ENTRY_OUT_LEN..fuse::ENTRY_OUT_LEN + 8]
                .try_into()
                .unwrap(),
        );
        Ok((nodeid, fh))
    }

    /// Opens a file the way a Linux guest does.
    ///
    /// The server answers OPEN with ENOSYS, which the kernel records once and
    /// then stops sending opens, closes and flushes altogether — so a real
    /// guest reads and writes with a file handle of zero and the inode doing
    /// the identifying. Modelling that here rather than papering over it is the
    /// point: these tests exercise the path production actually takes.
    fn open(&mut self, nodeid: u64, flags: u32) -> Result<u64, i32> {
        let mut body = Vec::new();
        body.extend_from_slice(&flags.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        match self.call(op::OPEN, nodeid, &body) {
            Err(38 /* ENOSYS */) => Ok(0),
            Err(other) => Err(other),
            Ok(reply) => Ok(u64::from_le_bytes(reply[0..8].try_into().unwrap())),
        }
    }

    fn write(&mut self, nodeid: u64, fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
        let mut body = Vec::new();
        body.extend_from_slice(&fh.to_le_bytes());
        body.extend_from_slice(&offset.to_le_bytes());
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // write_flags
        body.extend_from_slice(&0u64.to_le_bytes()); // lock_owner
        body.extend_from_slice(&0u32.to_le_bytes()); // flags
        body.extend_from_slice(&0u32.to_le_bytes()); // padding
        body.extend_from_slice(data);
        let reply = self.call(op::WRITE, nodeid, &body)?;
        Ok(u32::from_le_bytes(reply[0..4].try_into().unwrap()))
    }

    fn read(&mut self, nodeid: u64, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        let mut body = Vec::new();
        body.extend_from_slice(&fh.to_le_bytes());
        body.extend_from_slice(&offset.to_le_bytes());
        body.extend_from_slice(&size.to_le_bytes());
        body.resize(40, 0);
        self.call(op::READ, nodeid, &body)
    }

    fn opendir(&mut self, nodeid: u64) -> Result<u64, i32> {
        match self.call(op::OPENDIR, nodeid, &[0u8; 8]) {
            Err(38 /* ENOSYS */) => Ok(0),
            Err(other) => Err(other),
            Ok(reply) => Ok(u64::from_le_bytes(reply[0..8].try_into().unwrap())),
        }
    }

    /// Walks a directory to exhaustion, one page at a time, as the guest does.
    fn list(&mut self, nodeid: u64, page: u32, plus: bool) -> Vec<String> {
        let fh = self.opendir(nodeid).unwrap();
        let opcode = if plus { op::READDIRPLUS } else { op::READDIR };
        let mut names = Vec::new();
        let mut offset = 0u64;
        loop {
            let mut body = Vec::new();
            body.extend_from_slice(&fh.to_le_bytes());
            body.extend_from_slice(&offset.to_le_bytes());
            body.extend_from_slice(&page.to_le_bytes());
            body.resize(40, 0);
            let reply = self.call(opcode, nodeid, &body).unwrap();
            if reply.is_empty() {
                break;
            }
            let mut cursor = 0;
            while cursor < reply.len() {
                if plus {
                    cursor += fuse::ENTRY_OUT_LEN;
                }
                let off = u64::from_le_bytes(reply[cursor + 8..cursor + 16].try_into().unwrap());
                let namelen =
                    u32::from_le_bytes(reply[cursor + 16..cursor + 20].try_into().unwrap())
                        as usize;
                let start = cursor + fuse::DIRENT_HEADER_LEN;
                names.push(String::from_utf8_lossy(&reply[start..start + namelen]).into_owned());
                offset = off;
                cursor += fuse::dirent_len(namelen);
            }
        }
        names
    }

    fn host(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `O_CREAT | O_RDWR` in the guest's numbering, which is what the kernel puts
/// in a CREATE request.
const CREATE_RDWR: u32 = 0o100 | 0o2;

fn name_body(name: &str) -> Vec<u8> {
    let mut body = name.as_bytes().to_vec();
    body.push(0);
    body
}

#[test]
fn a_file_written_by_the_guest_appears_on_the_host() {
    let mut guest = Guest::new("write");
    let (nodeid, fh) = guest.create(1, "hello.txt", CREATE_RDWR).unwrap();
    assert_eq!(guest.write(nodeid, fh, 0, b"contents").unwrap(), 8);
    assert_eq!(
        std::fs::read_to_string(guest.host("hello.txt")).unwrap(),
        "contents"
    );
}

#[test]
fn a_file_written_by_the_host_is_read_by_the_guest() {
    let mut guest = Guest::new("read");
    std::fs::write(guest.host("from-host"), b"host wrote this").unwrap();
    let nodeid = guest.lookup(1, "from-host").unwrap();
    let fh = guest.open(nodeid, 0).unwrap();
    assert_eq!(guest.read(nodeid, fh, 0, 4096).unwrap(), b"host wrote this");
}

/// The bug this exists for: Linux `O_APPEND` is `O_TRUNC | O_EXCL` in macOS's
/// numbering. Forwarding the flag unchanged empties the file instead of
/// appending to it, and every step of the write still reports success.
#[test]
fn appending_does_not_truncate() {
    let mut guest = Guest::new("append");
    std::fs::write(guest.host("log"), b"first\n").unwrap();
    let nodeid = guest.lookup(1, "log").unwrap();
    let fh = guest
        .open(nodeid, 0o2001 /* O_WRONLY | O_APPEND */)
        .unwrap();
    // Without a reported open there is no append mode to remember, so the
    // guest kernel supplies the offset — which is what it does in practice.
    guest.write(nodeid, fh, 6, b"second\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(guest.host("log")).unwrap(),
        "first\nsecond\n"
    );
}

/// An inode is a descriptor, not a path, so a file the host renames out from
/// under the guest keeps working. This is what makes a `git checkout` on the
/// host not corrupt a build running in a container.
#[test]
fn an_open_file_survives_being_renamed_on_the_host() {
    let mut guest = Guest::new("rename-open");
    std::fs::write(guest.host("before"), b"payload").unwrap();
    let nodeid = guest.lookup(1, "before").unwrap();
    let fh = guest.open(nodeid, 0).unwrap();

    std::fs::rename(guest.host("before"), guest.host("after")).unwrap();

    assert_eq!(guest.read(nodeid, fh, 0, 4096).unwrap(), b"payload");
    // And the inode still resolves, which a remembered path would not.
    assert!(guest.call(op::GETATTR, nodeid, &[0u8; 16]).is_ok());
}

/// Unlink-while-open is ordinary POSIX and a great deal of software relies on
/// it. The guest's handle must keep reading and writing an inode with no name.
#[test]
fn an_open_file_survives_being_unlinked() {
    let mut guest = Guest::new("unlink-open");
    let (nodeid, fh) = guest.create(1, "doomed", CREATE_RDWR).unwrap();
    guest.write(nodeid, fh, 0, b"still here").unwrap();

    guest.call(op::UNLINK, 1, &name_body("doomed")).unwrap();
    // A miss is either ENOENT or a negative entry — a reply naming nodeid 0 —
    // depending on whether the share is cached. Both mean "not there".
    assert!(matches!(guest.lookup(1, "doomed"), Err(2) | Ok(0)));

    assert_eq!(guest.read(nodeid, fh, 0, 4096).unwrap(), b"still here");
    assert_eq!(guest.write(nodeid, fh, 10, b" and writable").unwrap(), 13);
    assert_eq!(
        guest.read(nodeid, fh, 0, 4096).unwrap(),
        b"still here and writable"
    );
}

/// Two names for one file are one inode. A guest that saw two would disagree
/// with its own `stat`, and every hard-link-based cache would break.
#[test]
fn a_hard_link_shares_its_nodeid() {
    let mut guest = Guest::new("link");
    let (original, _) = guest.create(1, "one", CREATE_RDWR).unwrap();
    let mut body = original.to_le_bytes().to_vec();
    body.extend_from_slice(&name_body("two"));
    let reply = guest.call(op::LINK, 1, &body).unwrap();
    let linked = u64::from_le_bytes(reply[0..8].try_into().unwrap());
    assert_eq!(linked, original);
    assert_eq!(guest.lookup(1, "two").unwrap(), original);
}

#[test]
fn symlinks_round_trip() {
    let mut guest = Guest::new("symlink");
    let mut body = name_body("link");
    body.extend_from_slice(&name_body("../target"));
    guest.call(op::SYMLINK, 1, &body).unwrap();
    let nodeid = guest.lookup(1, "link").unwrap();
    let target = guest.call(op::READLINK, nodeid, &[]).unwrap();
    assert_eq!(target, b"../target");
    // No terminator: the kernel takes the length from the header, and a
    // trailing NUL becomes part of the path.
    assert!(!target.ends_with(b"\0"));
}

/// A directory listing must not *repeat* an entry either, and that failure is
/// worse than losing one: `cp -a` remembers every `(dev, ino)` it has copied
/// and hard-links anything that comes back a second time, so a repeat makes a
/// recursive copy fail with "can't create link" for every file after it.
#[test]
fn a_listing_never_repeats_an_entry() {
    let mut guest = Guest::new("repeats");
    for index in 0..300 {
        std::fs::write(guest.host(&format!("entry-{index:03}")), b"x").unwrap();
    }
    // A page smaller than one READDIRPLUS record would make no progress at
    // all, which is a real constraint on the caller rather than on us: Linux
    // reads a directory a page at a time.
    for page in [512u32, 4096, 65536] {
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for name in guest.list(1, page, true) {
            *counts.entry(name).or_default() += 1;
        }
        let repeated: Vec<_> = counts.iter().filter(|(_, n)| **n > 1).collect();
        assert!(
            repeated.is_empty(),
            "page={page}: entries returned more than once: {repeated:?}"
        );
        assert_eq!(counts.len(), 302, "page={page}: 300 files plus . and ..");
    }
}

/// A directory listing must not lose entries at a page boundary. The failure is
/// silent and rate-dependent: with a big buffer every test passes.
#[test]
fn a_large_directory_lists_completely_across_pages() {
    let mut guest = Guest::new("readdir");
    for index in 0..200 {
        std::fs::write(guest.host(&format!("file-{index:03}")), b"").unwrap();
    }
    for (page, plus) in [(4096u32, false), (4096, true), (256, false), (256, true)] {
        let mut names = guest.list(1, page, plus);
        names.retain(|n| n != "." && n != "..");
        names.sort();
        assert_eq!(
            names.len(),
            200,
            "page={page} plus={plus} lost entries: got {}",
            names.len()
        );
        assert_eq!(names[0], "file-000");
        assert_eq!(names[199], "file-199");
    }
}

/// A rename storm on the host while the guest walks the same directory. The
/// requirement is not that the guest sees a particular state — it is that it
/// never sees a corrupt one, and never fails an operation it should not.
#[test]
fn concurrent_host_renames_do_not_corrupt_a_listing() {
    let mut guest = Guest::new("rename-storm");
    for index in 0..50 {
        std::fs::write(guest.host(&format!("stable-{index:02}")), b"x").unwrap();
    }
    std::fs::write(guest.host("moving"), b"payload").unwrap();

    let root = guest.root.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let storm = std::thread::spawn(move || {
        let mut current = String::from("moving");
        while !flag.load(std::sync::atomic::Ordering::Relaxed) {
            let next = format!("moving-{}", rand_suffix());
            if std::fs::rename(root.join(&current), root.join(&next)).is_ok() {
                current = next;
            }
        }
        current
    });

    for _ in 0..50 {
        let mut names = guest.list(1, 4096, true);
        names.retain(|n| n.starts_with("stable-"));
        assert_eq!(names.len(), 50, "stable entries must never disappear");
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let final_name = storm.join().unwrap();

    // Whatever it ended up called, it is still one file with its contents.
    let nodeid = guest.lookup(1, &final_name).unwrap();
    let fh = guest.open(nodeid, 0).unwrap();
    assert_eq!(guest.read(nodeid, fh, 0, 4096).unwrap(), b"payload");
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// The guest sizes its own reply buffers. A read must never promise more than
/// the chain can hold, whatever it asked for.
#[test]
fn a_read_is_clamped_to_the_buffer_the_guest_supplied() {
    let mut guest = Guest::new("clamp");
    let (nodeid, fh) = guest.create(1, "big", CREATE_RDWR).unwrap();
    guest.write(nodeid, fh, 0, &vec![b'z'; 8192]).unwrap();

    let mut body = Vec::new();
    body.extend_from_slice(&fh.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&8192u32.to_le_bytes());
    body.resize(40, 0);
    // A 1 KiB reply chain: the reply must be a short read, not an overrun.
    let reply = guest.call_with(op::READ, nodeid, &body, 1024).unwrap();
    assert_eq!(reply.len(), 1024 - fuse::OUT_HEADER_LEN);
}

/// Names that would leave the share must be refused before they reach a
/// syscall. A bind mount of one directory must not be a window onto the disk.
#[test]
fn names_cannot_escape_the_share() {
    let mut guest = Guest::new("escape");
    for name in ["..", ".", "", "../../etc/passwd"] {
        assert_eq!(
            guest.lookup(1, name),
            Err(22 /* EINVAL */),
            "{name:?} must be refused before it reaches a syscall"
        );
    }
}

#[test]
fn a_truncated_request_is_an_error_not_a_panic() {
    let mut guest = Guest::new("truncated");
    // WRITE claiming 4 GiB of payload it did not send.
    let mut body = Vec::new();
    body.extend_from_slice(&1u64.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&u32::MAX.to_le_bytes());
    body.resize(40, 0);
    assert!(guest.call(op::WRITE, 0, &body).is_err());
    // And an opcode with no body at all.
    assert!(guest.call(op::LOOKUP, 1, &[]).is_err());
}

/// A container runs as root and expects to own what it can see.
#[test]
fn host_ownership_is_presented_to_the_guest_as_root() {
    let mut guest = Guest::new("uid");
    std::fs::write(guest.host("owned"), b"x").unwrap();
    let nodeid = guest.lookup(1, "owned").unwrap();
    let reply = guest.call(op::GETATTR, nodeid, &[0u8; 16]).unwrap();
    // fuse_attr_out: 16 bytes of validity, then fuse_attr. uid is at offset 68
    // within the attr, which is 84 into the reply.
    let uid = u32::from_le_bytes(reply[84..88].try_into().unwrap());
    let gid = u32::from_le_bytes(reply[88..92].try_into().unwrap());
    assert_eq!(uid, 0, "the host user must appear as root inside the guest");
    assert_eq!(gid, 0);
}

/// Truncating through SETATTR is what `> file` does, and it is the one attribute
/// change that needs a writable descriptor the guest may not have supplied.
#[test]
fn setattr_can_truncate_without_a_handle() {
    let mut guest = Guest::new("truncate");
    std::fs::write(guest.host("shrink"), vec![b'x'; 4096]).unwrap();
    let nodeid = guest.lookup(1, "shrink").unwrap();

    let mut body = vec![0u8; 88];
    body[0..4].copy_from_slice(&fuse::fattr::SIZE.to_le_bytes());
    body[16..24].copy_from_slice(&10u64.to_le_bytes());
    guest.call(op::SETATTR, nodeid, &body).unwrap();

    assert_eq!(
        std::fs::metadata(guest.host("shrink")).unwrap().len(),
        10,
        "the file must actually be shorter on the host"
    );
}

/// `mkdir` then `rmdir` on a directory that is not empty. The errno is the
/// point: macOS `ENOTEMPTY` is 66, and 66 on Linux means something else.
#[test]
fn removing_a_non_empty_directory_reports_enotempty() {
    let mut guest = Guest::new("rmdir");
    let mut body = 0o755u32.to_le_bytes().to_vec();
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&name_body("dir"));
    guest.call(op::MKDIR, 1, &body).unwrap();
    std::fs::write(guest.host("dir").join("occupant"), b"").unwrap();

    assert_eq!(
        guest.call(op::RMDIR, 1, &name_body("dir")),
        Err(39),
        "the guest reads this number with Linux's table"
    );
}

#[test]
fn extended_attributes_round_trip() {
    let mut guest = Guest::new("xattr");
    std::fs::write(guest.host("tagged"), b"x").unwrap();
    let nodeid = guest.lookup(1, "tagged").unwrap();

    let mut body = Vec::new();
    body.extend_from_slice(&5u32.to_le_bytes()); // value size
    body.extend_from_slice(&0u32.to_le_bytes()); // flags
    body.extend_from_slice(&name_body("user.mark"));
    body.extend_from_slice(b"value");
    guest.call(op::SETXATTR, nodeid, &body).unwrap();

    let mut get = 64u32.to_le_bytes().to_vec();
    get.extend_from_slice(&0u32.to_le_bytes());
    get.extend_from_slice(&name_body("user.mark"));
    assert_eq!(guest.call(op::GETXATTR, nodeid, &get).unwrap(), b"value");

    guest
        .call(op::REMOVEXATTR, nodeid, &name_body("user.mark"))
        .unwrap();
    // macOS reports ENOATTR (93); the guest must read Linux's ENODATA (61).
    assert_eq!(guest.call(op::GETXATTR, nodeid, &get), Err(61));
}

/// Data written by the guest and fsynced must be on the host's disk, because
/// the durability gate kills the VMM immediately afterwards.
#[test]
fn fsync_reaches_the_host_file() {
    let mut guest = Guest::new("fsync");
    let (nodeid, fh) = guest.create(1, "durable", CREATE_RDWR).unwrap();
    guest.write(nodeid, fh, 0, b"committed").unwrap();
    let mut body = fh.to_le_bytes().to_vec();
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    guest.call(op::FSYNC, nodeid, &body).unwrap();
    assert_eq!(
        std::fs::read(guest.host("durable")).unwrap(),
        b"committed".to_vec()
    );
}

#[test]
fn statfs_describes_a_filesystem_with_room_on_it() {
    let mut guest = Guest::new("statfs");
    let reply = guest.call(op::STATFS, 1, &[]).unwrap();
    assert_eq!(reply.len(), 80);
    let blocks = u64::from_le_bytes(reply[0..8].try_into().unwrap());
    let bsize = u32::from_le_bytes(reply[40..44].try_into().unwrap());
    let namelen = u32::from_le_bytes(reply[44..48].try_into().unwrap());
    assert!(blocks > 0);
    assert!(bsize >= 512);
    assert_eq!(namelen, 255);
}

fn _unused(_: &Path) {}

/// Offsets into a `fuse_entry_out`'s embedded `fuse_attr`.
const ATTR_AT: usize = 40;
const INO_AT: usize = ATTR_AT;
const NLINK_AT: usize = ATTR_AT + 64;

/// Two different files must not look like two names for one file.
///
/// `cp -a` remembers every `(dev, ino)` it has copied and, for anything whose
/// link count is above one, links the second name to the first destination
/// rather than copying it again. Report the same inode number twice, or a link
/// count of two for a file with one name, and a recursive copy fails partway
/// through with "can't create link" — which is what it did.
#[test]
fn distinct_files_have_distinct_inode_numbers_and_one_link() {
    let mut guest = Guest::new("identity");
    let mut seen: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    for index in 0..200 {
        let name = format!("file-{index:03}");
        std::fs::write(guest.host(&name), b"x").unwrap();
        let mut body = name.as_bytes().to_vec();
        body.push(0);
        let reply = guest.call(op::LOOKUP, 1, &body).unwrap();

        let ino = u64::from_le_bytes(reply[INO_AT..INO_AT + 8].try_into().unwrap());
        let nlink = u32::from_le_bytes(reply[NLINK_AT..NLINK_AT + 4].try_into().unwrap());
        assert_eq!(nlink, 1, "{name} reported {nlink} links");
        if let Some(other) = seen.insert(ino, name.clone()) {
            panic!("{name} and {other} both report inode {ino}");
        }
    }
}

/// The same, through a directory listing rather than a lookup. READDIRPLUS
/// carries a dirent *and* an entry, and they have to agree about which inode a
/// name refers to — the dirent's number is what a caller sees from `readdir`
/// and the entry's is what it sees from `stat`.
#[test]
fn a_listing_and_a_lookup_agree_about_inode_numbers() {
    let mut guest = Guest::new("listing");
    for index in 0..40 {
        std::fs::write(guest.host(&format!("f{index:02}")), b"x").unwrap();
    }

    let fh = guest.opendir(1).unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&fh.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&65536u32.to_le_bytes());
    body.resize(40, 0);
    let reply = guest.call(op::READDIRPLUS, 1, &body).unwrap();

    let mut cursor = 0;
    while cursor + fuse::ENTRY_OUT_LEN + fuse::DIRENT_HEADER_LEN <= reply.len() {
        let entry = &reply[cursor..];
        let attr_ino = u64::from_le_bytes(entry[INO_AT..INO_AT + 8].try_into().unwrap());
        let dirent = &entry[fuse::ENTRY_OUT_LEN..];
        let dirent_ino = u64::from_le_bytes(dirent[0..8].try_into().unwrap());
        let namelen = u32::from_le_bytes(dirent[16..20].try_into().unwrap()) as usize;
        let name = String::from_utf8_lossy(
            &dirent[fuse::DIRENT_HEADER_LEN..fuse::DIRENT_HEADER_LEN + namelen],
        )
        .into_owned();
        let nodeid = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        // "." and ".." carry no entry, so there is nothing to agree with.
        if nodeid != 0 {
            assert_eq!(
                attr_ino, dirent_ino,
                "{name}: listing says inode {dirent_ino}, attributes say {attr_ino}"
            );
        }
        cursor += fuse::ENTRY_OUT_LEN + fuse::dirent_len(namelen);
    }
}

/// Reading a file whose descriptor was parked must return the file, not a
/// truncation of it.
///
/// The descriptor a share holds per inode is a cache of where the file lives,
/// and over budget the cold ones are closed and reopened by path on next use.
/// That machinery sits underneath every read, so a mistake in it does not
/// announce itself: it hands back a short read, and what surfaces two layers
/// up is a package manager reporting a syntax error at line 7784 of a lockfile
/// that is perfectly good on disk. Which is exactly how it surfaced.
///
/// The budget is forced low so that a few hundred files is enough to make the
/// reclaim run — at the real budget this would need a hundred thousand.
#[test]
fn a_parked_inode_still_reads_back_what_was_written() {
    // SAFETY: set before the server is built, and the value is read once.
    unsafe { std::env::set_var("LIGHTER_FS_FD_BUDGET", "64") };
    let mut guest = Guest::new("parked-read");

    const FILES: usize = 400;
    let content = |n: usize| format!("file {n} says the same thing every time\n").repeat(64);

    let mut ids = Vec::new();
    for n in 0..FILES {
        let name = format!("f{n}");
        let (nodeid, fh) = guest.create(1, &name, 0o2).unwrap();
        let body = content(n);
        assert_eq!(
            guest.write(nodeid, fh, 0, body.as_bytes()).unwrap() as usize,
            body.len()
        );
        ids.push((name, nodeid));
    }

    // The reclaim has to have actually run. Asserting only that the reads come
    // back right would pass just as well if nothing were ever parked, which is
    // how a reclaim that freed nothing at all survived a green test suite and
    // was found instead by `cp` reporting "No file descriptors available"
    // inside a guest.
    let (open, budget) = guest.server.descriptor_usage();
    assert!(
        open <= budget * 2,
        "the reclaim freed nothing: {open} descriptors against a budget of {budget}"
    );

    // Read them back in the order they were made, which is the order the
    // reclaim will have parked them in.
    for (n, (name, _)) in ids.iter().enumerate() {
        let expected = content(n);
        let nodeid = guest.lookup(1, name).unwrap();
        let fh = guest.open(nodeid, 0).unwrap();
        let got = guest.read(nodeid, fh, 0, expected.len() as u32).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&got),
            expected,
            "{name} came back wrong after its descriptor was parked"
        );
    }
    unsafe { std::env::remove_var("LIGHTER_FS_FD_BUDGET") };
}

/// The same, with the tree being walked while it is written.
///
/// This is the shape that actually broke: `cp -a` of a package tree creates
/// inodes as fast as it looks up the ones it already made, so nothing is cold
/// for long. A reclaim that only ever ages the front of each shard has most of
/// the table permanently out of reach, and the count climbs past the budget
/// while every sweep reports finding nothing to park.
#[test]
fn the_reclaim_keeps_up_while_the_tree_is_being_walked() {
    // SAFETY: set before the server is built, and the value is read once.
    unsafe { std::env::set_var("LIGHTER_FS_FD_BUDGET", "128") };
    let mut guest = Guest::new("parked-hot");

    const FILES: usize = 1500;
    let mut names = Vec::new();
    for n in 0..FILES {
        let name = format!("h{n}");
        let (nodeid, fh) = guest.create(1, &name, 0o2).unwrap();
        guest.write(nodeid, fh, 0, b"x").unwrap();
        names.push(name);
        // Touch a spread of what already exists, which is what keeps the
        // reference bits set.
        for step in [1usize, 7, 53, 211] {
            if let Some(earlier) = names.get(n.saturating_sub(step)) {
                guest.lookup(1, earlier).unwrap();
            }
        }
    }

    let (open, budget) = guest.server.descriptor_usage();
    unsafe { std::env::remove_var("LIGHTER_FS_FD_BUDGET") };
    assert!(
        open <= budget * 2,
        "the reclaim lost the race: {open} descriptors against a budget of {budget}"
    );
}

/// CREATE, returning the `open_flags` word of the reply as well, which is
/// where the server says whether it really created the file.
fn create_verbose(guest: &mut Guest, parent: u64, name: &str, flags: u32) -> Result<u32, i32> {
    let mut body = Vec::new();
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&0o644u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // umask
    body.extend_from_slice(&0u32.to_le_bytes()); // open_flags
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    let reply = guest.call(op::CREATE, parent, &body)?;
    Ok(u32::from_le_bytes(
        reply[fuse::ENTRY_OUT_LEN + 8..fuse::ENTRY_OUT_LEN + 12]
            .try_into()
            .unwrap(),
    ))
}

/// The guest may skip its pre-create LOOKUP (kernel patch 0004), which makes
/// "did this CREATE create?" a fact the server must report rather than one the
/// guest can assume — FMODE_CREATED skips the guest-side permission check, so
/// a wrong answer is a security bug in the guest, not a cosmetic one.
#[test]
fn create_reports_whether_it_created() {
    let mut guest = Guest::new("create-honesty");
    let first = create_verbose(&mut guest, 1, "fresh", CREATE_RDWR).unwrap();
    assert!(
        first & fuse::fopen::LIGHTER_CREATED != 0,
        "a create of a new file must say it created"
    );
    let second = create_verbose(&mut guest, 1, "fresh", CREATE_RDWR).unwrap();
    assert!(
        second & fuse::fopen::LIGHTER_CREATED == 0,
        "a create that opened an existing file must not claim otherwise"
    );
}

/// Linux refuses O_CREAT on an existing directory outright; macOS happily
/// opens one read-only, and with the pre-create LOOKUP skipped the guest no
/// longer discovers the directory first.
#[test]
fn create_refuses_a_directory() {
    let mut guest = Guest::new("create-on-dir");
    std::fs::create_dir(guest.host("subdir")).unwrap();
    let err = create_verbose(&mut guest, 1, "subdir", CREATE_RDWR).unwrap_err();
    assert_eq!(err, 21, "EISDIR, as open(2) itself would answer");
}

/// A trailing symlink belongs to the guest's VFS: the server must refuse to
/// walk it, and ELOOP is what sends the patched guest back to its ordinary
/// lookup path.
#[test]
fn create_refuses_a_symlink() {
    let mut guest = Guest::new("create-on-symlink");
    std::fs::write(guest.host("target"), b"real").unwrap();
    std::os::unix::fs::symlink("target", guest.host("alias")).unwrap();
    let err = create_verbose(&mut guest, 1, "alias", CREATE_RDWR).unwrap_err();
    assert_eq!(err, 40, "ELOOP, in the guest's numbering");
    // And the file it points at was neither truncated nor replaced.
    assert_eq!(std::fs::read(guest.host("target")).unwrap(), b"real");
}

/// O_EXCL still means what it says.
#[test]
fn create_excl_on_an_existing_file_is_eexist() {
    let mut guest = Guest::new("create-excl");
    std::fs::write(guest.host("taken"), b"").unwrap();
    const LINUX_O_EXCL: u32 = 0o200;
    let err = create_verbose(&mut guest, 1, "taken", CREATE_RDWR | LINUX_O_EXCL).unwrap_err();
    assert_eq!(err, 17, "EEXIST");
}
