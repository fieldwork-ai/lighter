//! Telling the guest to forget something.
//!
//! # Why this exists
//!
//! Everything the guest caches, it caches because we handed it a timeout, and
//! stock virtio-fs gives a server no way to take that back: Linux's driver has
//! a high-priority queue and request queues, both guest-to-host, so FUSE's
//! reverse channel has nowhere to go. A share can then only be cached for a
//! duration chosen in advance, and chosen for the worst case.
//!
//! That is the entire performance story. A metadata walk of an installed
//! package tree costs 275ms with a hundred-millisecond entry timeout and 15ms
//! with an unbounded one — against 62ms for the same walk on the Mac's own
//! disk. The difference is dentry revalidation and nothing else.
//!
//! So the guest kernel is patched to carry one more virtqueue, and this module
//! is what fills it. FSEvents says what the host changed; the registry says
//! whether the guest has ever heard of it; and a notification goes out saying
//! precisely which name or which inode to stop trusting.
//!
//! # The invariant that makes the timeouts safe
//!
//! **How long the guest may cache depends on whether we can correct it.** If
//! the channel is live the timeouts are long and staleness is measured in the
//! milliseconds FSEvents takes to notice; if it is not — an unpatched kernel,
//! or one that declined the feature — they fall back to short, and the share is
//! merely slower. Nothing has to be configured, and there is no combination of
//! versions that is fast and wrong.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// FUSE notification codes, from the same table the request opcodes come from.
mod code {
    pub const INVAL_INODE: i32 = 2;
    pub const INVAL_ENTRY: i32 = 3;
}

/// How many messages may wait for the guest to supply buffers.
///
/// Generous, because a `git checkout` on the host produces a burst and the
/// guest drains at its own pace — but bounded, because the producer is an
/// event stream we do not control. Overflow costs coherence, which is why the
/// timeout stays finite even when the channel is live: a message we had to drop
/// self-heals when the entry expires.
const BACKLOG: usize = 16 * 1024;

/// Something the guest should stop believing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// A name in a directory: it may have appeared, vanished, or come to mean
    /// a different file.
    Entry { parent: u64, name: Vec<u8> },
    /// A file's contents or attributes.
    Inode { nodeid: u64 },
}

impl Notification {
    /// The message as the guest's FUSE code expects to read it.
    ///
    /// Deliberately byte-identical to what a server writes to `/dev/fuse`: a
    /// `fuse_out_header` whose `unique` is zero and whose `error` carries the
    /// notification code. Reusing the format means the guest side has nothing
    /// to translate and no second parser to keep in step.
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(64);
        let code = match self {
            Notification::Entry { parent, name } => {
                body.extend_from_slice(&parent.to_le_bytes());
                body.extend_from_slice(&(name.len() as u32).to_le_bytes());
                body.extend_from_slice(&0u32.to_le_bytes()); // flags
                body.extend_from_slice(name);
                // The guest requires the terminator and checks for it; without
                // one it drops the message rather than reading past the name.
                body.push(0);
                code::INVAL_ENTRY
            }
            Notification::Inode { nodeid } => {
                body.extend_from_slice(&nodeid.to_le_bytes());
                // Offset -1 means "attributes only, keep the page cache"; zero
                // and zero means the whole file, which is what a host write
                // requires.
                body.extend_from_slice(&0i64.to_le_bytes());
                body.extend_from_slice(&0i64.to_le_bytes());
                code::INVAL_INODE
            }
        };

        let mut out = Vec::with_capacity(crate::fuse::OUT_HEADER_LEN + body.len());
        out.extend_from_slice(&((crate::fuse::OUT_HEADER_LEN + body.len()) as u32).to_le_bytes());
        out.extend_from_slice(&code.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes()); // unique: this is a notification
        out.extend_from_slice(&body);
        out
    }
}

/// The queue between the watcher and the device.
///
/// The watcher runs on a dispatch queue owned by Core Services and the device
/// is driven by vCPU threads; neither may block the other, so they meet here
/// and nowhere else.
#[derive(Default)]
pub struct Sink {
    pending: Mutex<VecDeque<Vec<u8>>>,
    waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    dropped: Mutex<u64>,
}

impl Sink {
    pub fn new() -> Sink {
        Sink::default()
    }

    /// Installs the callback that tells the transport there is something to
    /// send. Until one is installed, messages simply queue.
    pub fn set_waker(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        *self.waker.lock().expect("notify sink poisoned") = Some(wake);
    }

    /// Queues a notification and pokes the transport.
    pub fn push(&self, notification: Notification) {
        {
            let mut pending = self.pending.lock().expect("notify sink poisoned");
            if pending.len() >= BACKLOG {
                // The oldest goes, not the newest: a stale invalidation is
                // worth less than a fresh one, and the guest's own timeout is
                // the backstop for whichever is lost.
                pending.pop_front();
                *self.dropped.lock().expect("notify sink poisoned") += 1;
            }
            pending.push_back(notification.encode());
        }
        // Cloned out and the lock dropped before calling: the waker takes the
        // transport lock, and holding two is how this would deadlock against a
        // vCPU servicing a queue.
        let wake = self.waker.lock().expect("notify sink poisoned").clone();
        if let Some(wake) = wake {
            wake();
        }
    }

    /// The next message, if any.
    pub fn take(&self) -> Option<Vec<u8>> {
        self.pending
            .lock()
            .expect("notify sink poisoned")
            .pop_front()
    }

    /// Puts a message back, for when the guest had no buffer for it.
    pub fn unsend(&self, message: Vec<u8>) {
        self.pending
            .lock()
            .expect("notify sink poisoned")
            .push_front(message);
    }

    pub fn len(&self) -> usize {
        self.pending.lock().expect("notify sink poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many messages have been dropped for want of room. Diagnostics.
    pub fn dropped(&self) -> u64 {
        *self.dropped.lock().expect("notify sink poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guest checks for the terminator and drops a message without one, so
    /// this is not decoration.
    #[test]
    fn an_entry_notification_carries_a_terminated_name() {
        let bytes = Notification::Entry {
            parent: 7,
            name: b"README.md".to_vec(),
        }
        .encode();

        let len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let code = i32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let unique = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        assert_eq!(
            len,
            bytes.len(),
            "the header must describe the whole message"
        );
        assert_eq!(code, code::INVAL_ENTRY);
        assert_eq!(unique, 0, "a notification is not a reply to anything");

        assert_eq!(u64::from_le_bytes(bytes[16..24].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 9);
        assert_eq!(&bytes[32..41], b"README.md");
        assert_eq!(bytes[41], 0, "the name must be NUL-terminated");
    }

    #[test]
    fn an_inode_notification_covers_the_whole_file() {
        let bytes = Notification::Inode { nodeid: 42 }.encode();
        assert_eq!(
            i32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            code::INVAL_INODE
        );
        assert_eq!(u64::from_le_bytes(bytes[16..24].try_into().unwrap()), 42);
        assert_eq!(i64::from_le_bytes(bytes[24..32].try_into().unwrap()), 0);
        assert_eq!(i64::from_le_bytes(bytes[32..40].try_into().unwrap()), 0);
    }

    #[test]
    fn messages_come_back_in_the_order_they_went_in() {
        let sink = Sink::new();
        sink.push(Notification::Inode { nodeid: 1 });
        sink.push(Notification::Inode { nodeid: 2 });
        let first = sink.take().unwrap();
        assert_eq!(u64::from_le_bytes(first[16..24].try_into().unwrap()), 1);
        assert_eq!(sink.len(), 1);
    }

    /// A message the guest had no room for must not be lost.
    #[test]
    fn an_unsent_message_goes_back_to_the_front() {
        let sink = Sink::new();
        sink.push(Notification::Inode { nodeid: 1 });
        sink.push(Notification::Inode { nodeid: 2 });
        let first = sink.take().unwrap();
        sink.unsend(first);
        let again = sink.take().unwrap();
        assert_eq!(u64::from_le_bytes(again[16..24].try_into().unwrap()), 1);
    }

    /// The producer is an event stream nobody throttles. It must not be able
    /// to exhaust memory while the guest is busy.
    #[test]
    fn the_backlog_is_bounded() {
        let sink = Sink::new();
        for nodeid in 0..(BACKLOG as u64 + 500) {
            sink.push(Notification::Inode { nodeid });
        }
        assert_eq!(sink.len(), BACKLOG);
        assert_eq!(sink.dropped(), 500);
        // The oldest went, so the newest is still there to be delivered.
        let front = sink.take().unwrap();
        assert_eq!(
            u64::from_le_bytes(front[16..24].try_into().unwrap()),
            500,
            "the oldest messages should be the ones dropped"
        );
    }

    #[test]
    fn a_waker_is_called_when_something_arrives() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let sink = Sink::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        sink.set_waker(Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));
        sink.push(Notification::Inode { nodeid: 1 });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
