//! virtio-fs.
//!
//! The device that carries a host directory into the guest. It knows the
//! virtqueue mechanics and nothing about filesystems: a request chain is
//! copied out, handed to [`lighter_fs::Server`], and the reply is scattered
//! back into the chain's writable descriptors.
//!
//! # Why the work leaves the vCPU thread
//!
//! Every FUSE request is a macOS syscall, and a syscall on a vCPU thread stops
//! that core for its duration. A `pnpm install` issues hundreds of thousands of
//! them; served inline, the guest would spend most of its life not running, and
//! a single slow `stat` on a network volume would freeze the whole machine.
//!
//! So the vCPU only ever does bounded work: copy the request out of guest
//! memory, note where the reply goes, and post a job. A pool of host threads
//! does the syscalls and writes the replies straight into guest memory — which
//! is safe without any further synchronization because the descriptors are the
//! guest's promise not to touch those buffers until they come back on the used
//! ring. Only that last step needs the transport, and it happens through the
//! same waker the network and vsock devices use.
//!
//! # Ordering
//!
//! FUSE requires no ordering between concurrent requests; the guest serializes
//! anything that needs it. The one exception is FORGET, which must not overtake
//! the operations on the inode it forgets — and it cannot, because an inode is
//! reference-counted and an operation in flight holds a strong reference to it.

use std::collections::VecDeque;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use lighter_fs::{Server, Sink, SinkFull};

use crate::memory::GuestMemory;
use crate::virtio::mmio::COMMON_FEATURES;
use crate::virtio::queue::{Descriptor, Virtqueue};
use crate::virtio::{Serviced, VirtioDevice, device_type};

/// Queue indices, as the virtio-fs specification fixes them. There is no
/// notification queue because we do not offer `VIRTIO_FS_F_NOTIFICATION`.
pub const HIPRIO_QUEUE: u16 = 0;
pub const REQUEST_QUEUE: u16 = 1;

/// Longest mount tag the config space can hold.
pub const TAG_LEN: usize = 36;

/// How many host threads serve requests.
///
/// The work is syscall-bound rather than CPU-bound, so this is not a core
/// count: it is how many filesystem operations may be outstanding at once, and
/// a directory walk with everything cold wants rather more of them than the
/// machine has cores.
const WORKERS: usize = 8;

/// The callback a worker uses to poke the transport once a reply is ready.
///
/// Shared, optional and behind a lock because it is installed after the device
/// is built: the transport it pokes does not exist until every device has been
/// placed on the bus.
type Waker = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

/// A host directory, and the name the guest mounts it by.
#[derive(Debug, Clone)]
pub struct Share {
    pub tag: String,
    pub path: std::path::PathBuf,
}

/// Where a reply goes: the writable half of one descriptor chain.
struct ChainSink {
    memory: Arc<GuestMemory>,
    segments: Vec<(u64, u32)>,
    /// Index into `segments`, and how far into that segment we are.
    at: usize,
    offset: u32,
    remaining: usize,
}

impl ChainSink {
    fn new(memory: Arc<GuestMemory>, segments: Vec<(u64, u32)>) -> ChainSink {
        let remaining = segments.iter().map(|(_, len)| *len as usize).sum();
        ChainSink {
            memory,
            segments,
            at: 0,
            offset: 0,
            remaining,
        }
    }
}

impl Sink for ChainSink {
    fn capacity(&self) -> usize {
        self.remaining
    }

    fn write(&mut self, mut data: &[u8]) -> Result<(), SinkFull> {
        if data.len() > self.remaining {
            return Err(SinkFull);
        }
        while !data.is_empty() {
            let (addr, len) = *self.segments.get(self.at).ok_or(SinkFull)?;
            let room = (len - self.offset) as usize;
            if room == 0 {
                self.at += 1;
                self.offset = 0;
                continue;
            }
            let take = room.min(data.len());
            // A write that fails means the guest gave us an address outside its
            // own memory. There is nothing to do but stop; the reply will be
            // short and the guest will see a malformed answer, which is the
            // correct consequence of a malformed request.
            if self
                .memory
                .write(addr + u64::from(self.offset), &data[..take])
                .is_err()
            {
                return Err(SinkFull);
            }
            self.offset += take as u32;
            self.remaining -= take;
            data = &data[take..];
        }
        Ok(())
    }
}

/// One request, detached from the guest so a host thread may work on it.
struct Job {
    head: u16,
    request: Vec<u8>,
    reply: Vec<(u64, u32)>,
}

/// A finished request, waiting to go back on the used ring.
struct Completion {
    head: u16,
    len: u32,
}

/// State shared between the vCPU threads and the worker pool.
struct Pool {
    jobs: Sender<Job>,
    done: Arc<Mutex<VecDeque<Completion>>>,
    _threads: Vec<std::thread::JoinHandle<()>>,
}

impl Pool {
    fn start(server: Arc<Server>, memory: Arc<GuestMemory>, tag: String, waker: Waker) -> Pool {
        let (jobs, receiver) = channel::<Job>();
        let receiver = Arc::new(Mutex::new(receiver));
        let done: Arc<Mutex<VecDeque<Completion>>> = Arc::new(Mutex::new(VecDeque::new()));

        let mut threads = Vec::with_capacity(WORKERS);
        for index in 0..WORKERS {
            let receiver = receiver.clone();
            let server = server.clone();
            let memory = memory.clone();
            let done = done.clone();
            let waker = waker.clone();
            let handle = std::thread::Builder::new()
                .name(format!("fs-{tag}-{index}"))
                .spawn(move || {
                    loop {
                        // The lock is held only across `recv`, so workers block
                        // on the channel rather than on each other.
                        let job = {
                            let guard = receiver.lock().expect("fs job queue poisoned");
                            guard.recv()
                        };
                        let Ok(job) = job else { break };

                        let mut sink = ChainSink::new(memory.clone(), job.reply);
                        let written = server.dispatch(&job.request, &mut sink);
                        done.lock()
                            .expect("fs completions poisoned")
                            .push_back(Completion {
                                head: job.head,
                                len: written as u32,
                            });

                        // Clone the waker out and drop the lock before calling
                        // it: it takes the transport lock, and holding two is
                        // how this deadlocks against a vCPU servicing a queue.
                        let wake = waker.lock().expect("fs waker poisoned").clone();
                        if let Some(wake) = wake {
                            wake();
                        }
                    }
                })
                .expect("failed to spawn a filesystem worker");
            threads.push(handle);
        }

        Pool {
            jobs,
            done,
            _threads: threads,
        }
    }
}

/// The device.
pub struct Fs {
    tag: String,
    server: Arc<Server>,
    /// Built at activation, because the worker threads need guest memory and
    /// the device does not have it until the driver is ready.
    pool: Option<Pool>,
    /// Held so the vCPU-served queue can build a reply sink of its own.
    memory: Option<Arc<GuestMemory>>,
    waker: Waker,
}

impl Fs {
    pub fn new(share: &Share) -> std::io::Result<Fs> {
        if share.tag.len() > TAG_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "share tag {:?} is longer than the {TAG_LEN} bytes the device can advertise",
                    share.tag
                ),
            ));
        }
        let server = Server::new(&share.path)?;
        Ok(Fs {
            tag: share.tag.clone(),
            server: Arc::new(server),
            pool: None,
            memory: None,
            waker: Arc::new(Mutex::new(None)),
        })
    }

    /// The slot a worker reads its wake-up callback from.
    ///
    /// Handed out before the device is boxed, because the transport it has to
    /// poke does not exist until every device has been placed on the bus. The
    /// workers hold this same `Arc`, so filling it later is enough — there is
    /// no copy to keep in step.
    pub fn waker(&self) -> Waker {
        self.waker.clone()
    }

    /// Splits a chain into the request the guest wrote and the buffers it left
    /// for the reply.
    ///
    /// FUSE puts every readable descriptor first and every writable one after,
    /// so this is a partition rather than a parse — but it is done by the
    /// descriptor flags rather than by position, because trusting the guest's
    /// ordering would let a malformed chain make us write into a buffer it
    /// meant us to read.
    fn split(memory: &GuestMemory, chain: impl Iterator<Item = Descriptor>) -> (Vec<u8>, Vec<(u64, u32)>) {
        let mut request = Vec::new();
        let mut reply = Vec::new();
        for desc in chain {
            if desc.is_write_only() {
                reply.push((desc.addr, desc.len));
            } else {
                let at = request.len();
                request.resize(at + desc.len as usize, 0);
                if memory.read(desc.addr, &mut request[at..]).is_err() {
                    request.truncate(at);
                }
            }
        }
        (request, reply)
    }

    /// Returns finished requests to the driver.
    fn reap(&mut self, queue: &mut Virtqueue, memory: &GuestMemory) -> bool {
        let Some(pool) = &self.pool else {
            return false;
        };
        let mut any = false;
        loop {
            let completion = pool
                .done
                .lock()
                .expect("fs completions poisoned")
                .pop_front();
            let Some(completion) = completion else { break };
            queue.push_used(memory, completion.head, completion.len);
            any = true;
        }
        any
    }
}

impl VirtioDevice for Fs {
    fn device_type(&self) -> u32 {
        device_type::FS
    }

    fn name(&self) -> &'static str {
        "fs"
    }

    fn features(&self) -> u64 {
        COMMON_FEATURES
    }

    fn queue_count(&self) -> usize {
        2
    }

    fn config_read(&self, offset: u64, data: &mut [u8]) {
        // `struct virtio_fs_config`: a 36-byte NUL-padded tag, the number of
        // request queues, and a notification buffer size we leave at zero
        // because we do not offer notifications.
        let mut config = [0u8; TAG_LEN + 8];
        let tag = self.tag.as_bytes();
        config[..tag.len()].copy_from_slice(tag);
        config[TAG_LEN..TAG_LEN + 4].copy_from_slice(&1u32.to_le_bytes());
        let start = offset as usize;
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = config.get(start + index).copied().unwrap_or(0);
        }
    }

    fn activate(&mut self, mem: Arc<GuestMemory>) {
        if self.pool.is_some() {
            return;
        }
        self.memory = Some(mem.clone());
        self.pool = Some(Pool::start(
            self.server.clone(),
            mem,
            self.tag.clone(),
            self.waker.clone(),
        ));
        tracing::info!(tag = %self.tag, root = %self.server.root().display(), "share activated");
    }

    fn notify(&mut self, queue: u16, queues: &mut [Virtqueue], mem: &GuestMemory) -> Serviced {
        match queue {
            HIPRIO_QUEUE => {
                // The high-priority queue carries FORGET and INTERRUPT: bounded,
                // allocation-free, and required not to queue behind a slow
                // `stat`. They are served here, on the vCPU, deliberately.
                let Some(memory) = self.memory.clone() else {
                    return Serviced::NONE;
                };
                let Some(hiprio) = queues.get_mut(HIPRIO_QUEUE as usize) else {
                    return Serviced::NONE;
                };
                let mut used = false;
                while let Some(chain) = hiprio.pop(mem) {
                    let head = chain.head();
                    let (request, reply) = Fs::split(mem, chain);
                    let mut sink = ChainSink::new(memory.clone(), reply);
                    let written = self.server.dispatch(&request, &mut sink);
                    hiprio.push_used(mem, head, written as u32);
                    used = true;
                }
                Serviced::queue_if(HIPRIO_QUEUE, used)
            }
            REQUEST_QUEUE => {
                let Some(pool_present) = self.pool.as_ref().map(|p| p.jobs.clone()) else {
                    return Serviced::NONE;
                };
                let Some(requests) = queues.get_mut(REQUEST_QUEUE as usize) else {
                    return Serviced::NONE;
                };
                while let Some(chain) = requests.pop(mem) {
                    let head = chain.head();
                    let (request, reply) = Fs::split(mem, chain);
                    if pool_present
                        .send(Job {
                            head,
                            request,
                            reply,
                        })
                        .is_err()
                    {
                        // Every worker is gone. Return the chain rather than
                        // leaking it, so the guest sees an error instead of a
                        // hang.
                        requests.push_used(mem, head, 0);
                    }
                }
                let used = self.reap(&mut queues[REQUEST_QUEUE as usize], mem);
                Serviced::queue_if(REQUEST_QUEUE, used)
            }
            _ => Serviced::NONE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tag_is_advertised_nul_padded() {
        let share = Share {
            tag: "workspace".into(),
            path: std::env::temp_dir(),
        };
        let fs = Fs::new(&share).unwrap();
        let mut config = [0xffu8; TAG_LEN + 8];
        fs.config_read(0, &mut config);
        assert_eq!(&config[..9], b"workspace");
        assert!(
            config[9..TAG_LEN].iter().all(|&b| b == 0),
            "the tag must be NUL-padded or the guest mounts a name with rubbish on the end"
        );
        assert_eq!(
            u32::from_le_bytes(config[TAG_LEN..TAG_LEN + 4].try_into().unwrap()),
            1,
            "one request queue"
        );
    }

    /// A partial read of config space is ordinary — Linux reads the tag and the
    /// queue count separately — and must not run off the end.
    #[test]
    fn config_reads_past_the_end_are_zero_rather_than_out_of_bounds() {
        let fs = Fs::new(&Share {
            tag: "t".into(),
            path: std::env::temp_dir(),
        })
        .unwrap();
        let mut data = [0xffu8; 8];
        fs.config_read(TAG_LEN as u64 + 4, &mut data);
        assert_eq!(data, [0u8; 8]);
    }

    #[test]
    fn a_tag_too_long_for_the_config_space_is_refused() {
        let long = "x".repeat(TAG_LEN + 1);
        assert!(
            Fs::new(&Share {
                tag: long,
                path: std::env::temp_dir()
            })
            .is_err()
        );
    }
}
