//! A network volume whose server refuses to open more files.
//!
//! Its own binary because it lowers this process's descriptor limit, which
//! every test in a binary shares: the opens the share makes then fail with
//! EMFILE exactly as they do once a NAS has no handle left to give (#93),
//! and nothing else is running to be failed alongside them.

use std::os::fd::RawFd;

use lighter_fs::fuse::{self, op};
use lighter_fs::{FillError, Server, Sink, SinkFull};

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

    fn read(&mut self, nodeid: u64) -> Result<Vec<u8>, i32> {
        let mut body = Vec::new();
        body.extend_from_slice(&0u64.to_le_bytes()); // fh
        body.extend_from_slice(&0u64.to_le_bytes()); // offset
        body.extend_from_slice(&4096u32.to_le_bytes());
        body.resize(40, 0);
        self.call(op::READ, nodeid, &body)
    }
}

fn open_descriptors() -> u64 {
    std::fs::read_dir("/dev/fd").unwrap().count() as u64
}

/// A server out of handles is one the share stops holding handles on: the
/// share lets go of what it holds there, lowers the volume's allowance, and
/// serves the request again. Every lookup and read of a walk succeeds with
/// the share's opens failing all the way through it.
#[test]
fn a_server_out_of_handles_is_let_go_of_and_asked_again() {
    let root = std::env::temp_dir().join(format!("lighter-fs-refusal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    const DIRS: usize = 150;
    const FILES: usize = 4;
    for d in 0..DIRS {
        let dir = root.join(format!("d{d}"));
        std::fs::create_dir(&dir).unwrap();
        for f in 0..FILES {
            std::fs::write(dir.join(format!("f{f}")), format!("{d}/{f}")).unwrap();
        }
    }
    let pool = lighter_fs::inode::Pool::default();
    let server = Server::new_in_pool(&root, &pool).unwrap();
    let mut guest = Guest { server, unique: 1 };
    let mut init = Vec::new();
    init.extend_from_slice(&7u32.to_le_bytes());
    init.extend_from_slice(&38u32.to_le_bytes());
    init.extend_from_slice(&(128u32 * 1024).to_le_bytes());
    init.extend_from_slice(&u32::MAX.to_le_bytes());
    init.resize(64, 0);
    guest.call(op::INIT, 0, &init).unwrap();
    guest.server.assume_network_share();
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(&root).unwrap().dev() as i64;
    let remote = pool
        .remote(dev)
        .expect("the share is taken for a network volume");
    let (dirs_allowed, files_allowed) = remote.allowance();

    // Room for forty more descriptors: a server that runs out long before
    // the volume's allowance of 128 directories and 64 files is reached.
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: an output buffer of the right type.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0
    );
    let tight = libc::rlimit {
        rlim_cur: open_descriptors() + 40,
        rlim_max: limit.rlim_max,
    };
    // SAFETY: a limit no higher than the hard one.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &tight) }, 0);

    let mut failures = Vec::new();
    for d in 0..DIRS {
        let dir = match guest.lookup(1, &format!("d{d}")) {
            Ok(dir) => dir,
            Err(errno) => {
                failures.push(format!("lookup d{d}: {errno}"));
                continue;
            }
        };
        for f in 0..FILES {
            match guest
                .lookup(dir, &format!("f{f}"))
                .and_then(|file| guest.read(file))
            {
                Ok(read) if read == format!("{d}/{f}").as_bytes() => {}
                Ok(read) => failures.push(format!("d{d}/f{f}: read {read:?}")),
                Err(errno) => failures.push(format!("d{d}/f{f}: {errno}")),
            }
        }
    }

    // SAFETY: restoring the limit read above.
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
    let _ = std::fs::remove_dir_all(&root);
    assert!(failures.is_empty(), "the walk failed: {failures:?}");
    assert!(
        remote.refusals() > 0,
        "the server never ran out: the test tested nothing"
    );
    let (dirs, files) = remote.allowance();
    assert!(
        dirs < dirs_allowed && files < files_allowed,
        "the allowance stayed at {dirs} directories and {files} files after {} refusals",
        remote.refusals()
    );
}
