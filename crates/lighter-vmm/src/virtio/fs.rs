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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use lighter_fs::{Server, Sink, SinkFull};

use crate::memory::GuestMemory;
use crate::virtio::mmio::COMMON_FEATURES;
use crate::virtio::queue::{Descriptor, Virtqueue};
use crate::virtio::{Serviced, VirtioDevice, device_type};

/// Queue indices, as the virtio-fs specification fixes them. There is no
/// notification queue because we do not offer `VIRTIO_FS_F_NOTIFICATION`.
pub const HIPRIO_QUEUE: u16 = 0;
pub const REQUEST_QUEUE: u16 = 1;
/// The queue *we* write to, carrying invalidations into the guest.
///
/// Last rather than second, so that a guest which does not know about it — an
/// unpatched kernel — agrees with us about every other index and simply never
/// makes this one ready.
pub const NOTIFY_QUEUE: u16 = 2;

/// Feature bit: the device can push FUSE notifications.
///
/// Not in any specification. It is ours, matched by a patch to the guest's
/// virtio-fs driver (`guest/kernel/patches/`), and it is what lets the server
/// hand out cache lifetimes measured in seconds instead of milliseconds.
const VIRTIO_FS_F_NOTIFICATION: u64 = 1 << 0;

/// Longest mount tag the config space can hold.
pub const TAG_LEN: usize = 36;

/// How many host threads serve requests.
///
/// The work is syscall-bound rather than CPU-bound, so this is not a core
/// count: it is how many filesystem operations may be outstanding at once, and
/// a package install — dozens of processes each blocked on a `stat` — wants
/// rather more of them than the machine has cores.
const WORKERS: usize = 16;

/// The callback a worker uses to poke the transport once a reply is ready.
///
/// Shared, optional and behind a lock because it is installed after the device
/// is built: the transport it pokes does not exist until every device has been
/// placed on the bus.
type Waker = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

/// When a request is served on the vCPU thread instead of a worker.
///
/// The hand-off to a worker costs a wake-up and a scheduler hop, and the guest
/// is now spinning for its own reply — so for a short operation, serving it
/// right here is strictly faster. The reason not to always do it is that a slow
/// syscall then stops a core rather than a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inline {
    /// Never; every request goes to the pool.
    Never,
    /// Only when no other request is waiting, so one slow call cannot become a
    /// queue that everything else sits behind.
    WhenIdle,
    /// Always. Fastest when the guest is spinning, at the cost of a vCPU
    /// stalling on whatever macOS takes to answer.
    Always,
}

impl Inline {
    fn from_env() -> Inline {
        match std::env::var("LIGHTER_FS_INLINE").as_deref() {
            Ok("0") | Ok("never") => Inline::Never,
            Ok("always") => Inline::Always,
            _ => Inline::WhenIdle,
        }
    }

    fn applies(&self, queue: &Queue) -> bool {
        match self {
            Inline::Never => false,
            Inline::WhenIdle => queue.is_empty(),
            Inline::Always => true,
        }
    }
}

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

/// Jobs waiting for a worker.
///
/// A `Mutex<Receiver>` around a channel would be the obvious thing and is
/// quietly disastrous: a worker blocked in `recv` holds the mutex, so only one
/// worker can ever be *waiting*, and every job dispatch becomes a mutex handoff
/// between two threads. Measured on a package install, that alone was most of a
/// twenty-microsecond round trip across a hundred thousand requests.
///
/// Here the lock is held only long enough to push or pop; waiting happens on
/// the condition variable, where any number of workers can wait at once.
struct Queue {
    jobs: Mutex<Option<VecDeque<Job>>>,
    arrived: Condvar,
}

impl Queue {
    fn new() -> Queue {
        Queue {
            jobs: Mutex::new(Some(VecDeque::new())),
            arrived: Condvar::new(),
        }
    }

    /// Returns false once the queue has been closed.
    fn push(&self, job: Job) -> bool {
        let mut guard = self.jobs.lock().expect("fs job queue poisoned");
        let Some(queue) = guard.as_mut() else {
            return false;
        };
        queue.push_back(job);
        drop(guard);
        self.arrived.notify_one();
        true
    }

    /// Blocks until there is a job, or until the queue is closed.
    fn pop(&self) -> Option<Job> {
        let mut guard = self.jobs.lock().expect("fs job queue poisoned");
        loop {
            if let Some(job) = guard.as_mut()?.pop_front() {
                return Some(job);
            }
            guard = self.arrived.wait(guard).expect("fs job queue poisoned");
        }
    }

    /// Whether a worker would find nothing to do.
    fn is_empty(&self) -> bool {
        self.jobs
            .lock()
            .expect("fs job queue poisoned")
            .as_ref()
            .is_none_or(|queue| queue.is_empty())
    }

    /// Wakes every worker and lets them retire.
    fn close(&self) {
        *self.jobs.lock().expect("fs job queue poisoned") = None;
        self.arrived.notify_all();
    }
}

/// State shared between the vCPU threads and the worker pool.
/// Retiring a pool closes its queue, which is what lets its threads exit rather
/// than waiting forever on a condition variable nothing will signal.
impl Drop for Pool {
    fn drop(&mut self) {
        self.queue.close();
    }
}

struct Pool {
    queue: Arc<Queue>,
    done: Arc<Mutex<VecDeque<Completion>>>,
    /// Whether a wake-up is already on its way to the transport.
    ///
    /// Without this, twenty-four workers finishing at once raise twenty-four
    /// interrupts for one batch of replies, and each takes the transport lock
    /// on the way. The guest only needs to be told once that there is
    /// something on the used ring.
    waking: Arc<AtomicBool>,
    _threads: Vec<std::thread::JoinHandle<()>>,
}

impl Pool {
    fn start(server: Arc<Server>, memory: Arc<GuestMemory>, tag: String, waker: Waker) -> Pool {
        let queue = Arc::new(Queue::new());
        let done: Arc<Mutex<VecDeque<Completion>>> = Arc::new(Mutex::new(VecDeque::new()));
        let waking = Arc::new(AtomicBool::new(false));

        let mut threads = Vec::with_capacity(WORKERS);
        for index in 0..WORKERS {
            let queue = queue.clone();
            let server = server.clone();
            let memory = memory.clone();
            let done = done.clone();
            let waker = waker.clone();
            let waking = waking.clone();
            let handle = std::thread::Builder::new()
                .name(format!("fs-{tag}-{index}"))
                .spawn(move || {
                    while let Some(job) = queue.pop() {

                        let mut sink = ChainSink::new(memory.clone(), job.reply);
                        let written = server.dispatch(&job.request, &mut sink);
                        done.lock()
                            .expect("fs completions poisoned")
                            .push_back(Completion {
                                head: job.head,
                                len: written as u32,
                            });

                        // One wake per batch. `notify` clears the flag *before*
                        // draining, so a completion queued in the gap sets it
                        // again and gets its own wake-up — there is no ordering
                        // in which a reply is left sitting on the ring.
                        if waking.swap(true, Ordering::AcqRel) {
                            continue;
                        }
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
            queue,
            done,
            waking,
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
    /// Whether the guest negotiated the notification queue.
    notifications: bool,
    /// When a request may be served on the vCPU thread rather than handed to a
    /// worker.
    inline: Inline,
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
            notifications: false,
            inline: Inline::from_env(),
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

    /// The queue of invalidations waiting to reach the guest.
    pub fn notifications(&self) -> Arc<lighter_fs::notify::Sink> {
        self.server.notifications().clone()
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

    /// Moves queued invalidations into the buffers the guest posted.
    ///
    /// The direction is the unusual part: on every other queue the guest
    /// supplies a request and we fill in the reply, but here it supplies empty
    /// buffers and we decide when there is something to put in them. A message
    /// that does not fit the buffer it was given is dropped rather than
    /// truncated — half a notification names the wrong file.
    fn deliver(&mut self, queues: &mut [Virtqueue], mem: &GuestMemory) -> bool {
        if !self.notifications {
            return false;
        }
        let Some(memory) = self.memory.clone() else {
            return false;
        };
        let Some(queue) = queues.get_mut(NOTIFY_QUEUE as usize) else {
            return false;
        };
        let sink = self.server.notifications();
        let mut delivered = false;
        while !sink.is_empty() {
            let Some(chain) = queue.pop(mem) else { break };
            let head = chain.head();
            let (_, buffers) = Fs::split(mem, chain);
            let Some(message) = sink.take() else {
                // Nothing left after all; give the buffer back untouched so the
                // guest can re-post it.
                queue.push_used(mem, head, 0);
                delivered = true;
                break;
            };
            let mut sink_out = ChainSink::new(memory.clone(), buffers);
            let len = message.len();
            if sink_out.capacity() < len || sink_out.write(&message).is_err() {
                tracing::warn!(
                    len,
                    capacity = sink_out.capacity(),
                    "a notification did not fit the buffer the guest posted"
                );
                queue.push_used(mem, head, 0);
            } else {
                queue.push_used(mem, head, len as u32);
            }
            delivered = true;
        }
        delivered
    }

    /// Returns finished requests to the driver.
    fn reap(&mut self, queue: &mut Virtqueue, memory: &GuestMemory) -> bool {
        let Some(pool) = &self.pool else {
            return false;
        };
        pool.waking.store(false, Ordering::Release);
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
        COMMON_FEATURES | VIRTIO_FS_F_NOTIFICATION
    }

    fn ack_features(&mut self, features: u64) {
        // Recorded, not acted on. The driver writes its feature set one 32-bit
        // half at a time and in whichever order it likes, so this is called
        // with a partial answer at least once; the set is only final at
        // DRIVER_OK, which is where the decision is made.
        self.notifications = features & VIRTIO_FS_F_NOTIFICATION != 0;
    }

    fn queue_count(&self) -> usize {
        3
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
        // What the guest accepted decides how long the server will let it cache
        // anything, because a lifetime we cannot withdraw has to be short.
        self.server.set_push_invalidation(self.notifications);
        if !self.notifications {
            tracing::info!(
                tag = %self.tag,
                "guest declined the notification queue; caching conservatively"
            );
        }
        // A histogram on an interval, when asked for. Tuning a filesystem
        // without one is a sequence of rebuild-boot-measure cycles spent
        // disproving theories that a single run would have settled.
        if self.server.stats_enabled() {
            let server = self.server.clone();
            let tag = self.tag.clone();
            std::thread::Builder::new()
                .name(format!("fs-{tag}-stats"))
                .spawn(move || {
                    // `LIGHTER_FS_STATS` doubles as the interval in seconds, so
                    // a short workload can be given windows it fits inside.
                    let every = std::env::var("LIGHTER_FS_STATS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .filter(|seconds| *seconds > 0)
                        .unwrap_or(5);
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(every));
                        server.log_stats();
                    }
                })
                .expect("failed to spawn the filesystem stats thread");
        }
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
                let (Some(queue), Some(memory)) = (
                    self.pool.as_ref().map(|pool| pool.queue.clone()),
                    self.memory.clone(),
                ) else {
                    return Serviced::NONE;
                };
                let Some(requests) = queues.get_mut(REQUEST_QUEUE as usize) else {
                    return Serviced::NONE;
                };
                let mut used = false;
                while let Some(chain) = requests.pop(mem) {
                    let head = chain.head();
                    let (request, reply) = Fs::split(mem, chain);

                    // The lone request in a quiet moment is served right here,
                    // on the vCPU that asked for it.
                    //
                    // This is the difference between a round trip and a round
                    // trip plus two context switches, and on a workload that
                    // waits for each answer before asking the next question —
                    // which is most of what a package manager does — the
                    // context switches were the larger half. The pool still
                    // exists and still matters: the moment a second request is
                    // outstanding, everything goes to it, so a slow syscall
                    // stalls one core for one operation rather than becoming a
                    // queue everything else waits behind.
                    if self.inline.applies(&queue) {
                        let mut sink = ChainSink::new(memory.clone(), reply);
                        let written = self.server.dispatch(&request, &mut sink);
                        requests.push_used(mem, head, written as u32);
                        used = true;
                        continue;
                    }

                    if !queue.push(Job {
                        head,
                        request,
                        reply,
                    }) {
                        // Every worker is gone. Return the chain rather than
                        // leaking it, so the guest sees an error instead of a
                        // hang.
                        requests.push_used(mem, head, 0);
                        used = true;
                    }
                }
                let reaped = self.reap(&mut queues[REQUEST_QUEUE as usize], mem);
                Serviced::queue_if(REQUEST_QUEUE, used || reaped)
            }
            NOTIFY_QUEUE => Serviced::queue_if(NOTIFY_QUEUE, self.deliver(queues, mem)),
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
