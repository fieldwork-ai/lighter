//! File sharing between the host and the guest.
//!
//! # The secret this crate keeps
//!
//! How a host directory becomes a directory inside the VM: the FUSE protocol
//! the guest's virtio-fs driver speaks, the macOS syscalls that answer it, and
//! the identity and caching policy applied in between. The VMM knows only that
//! there is something which turns a request buffer into a reply buffer.
//!
//! That seam is deliberate and load-bearing. Milestone 5 replaces the caching
//! policy wholesale and adds a host change-notification channel; none of that
//! should be visible to the device model, and the device model's virtqueue
//! mechanics should never be visible here.
//!
//! ```text
//!   server    one request in, one reply out
//!     ├── fuse     the wire format, and nothing else
//!     ├── inode    what a nodeid and an fh mean
//!     ├── sys      the only unsafe code, one libc call per function
//!     └── errno    macOS error numbers as Linux ones
//! ```

pub mod errno;
pub mod fuse;
pub mod inode;
pub mod server;
pub mod sys;

pub use server::{MAX_WRITE, Server, Sink, SinkFull};
