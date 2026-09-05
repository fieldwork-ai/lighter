//! The lighter machine.
//!
//! # The secret this crate keeps
//!
//! What the guest's machine *is*, as a Virtualization.framework configuration
//! (`vz`), and the host half of every channel into it: the link the network
//! and the file server ride (`link`), the balloon the memory comes back
//! through (`balloon`, `memory_policy`), the shares (`share`), the published
//! ports (`streams`). It knows nothing about FUSE (`lighter-fs` does) or
//! Docker's API (`lighter-docker` does).
//!
//! # Layering
//!
//! ```text
//!   machine   assembles the machine and starts the channels
//!     ├── vz             the framework: boot, devices, lifecycle, the balloon knob
//!     ├── disk           the image files the block devices serve
//!     ├── link           the card: responder, smoltcp, and the reactor over it
//!     │     ├── net      DHCP and echo answered in process
//!     │     ├── dns      the Mac's resolver, for the guest
//!     │     └── share    FUSE requests to lighter-fs and replies back
//!     ├── streams        published ports bound on the Mac
//!     ├── balloon        the one number the framework's balloon takes
//!     └── memory_policy  what that number should be
//! ```

pub mod balloon;
pub mod disk;
pub mod dns;
pub mod footprint;
pub mod link;
pub mod machine;
pub mod memory_policy;
pub mod mempressure;
pub mod net;
pub mod qos;
pub mod share;
pub mod sockbuf;
pub mod streams;
pub mod vz;
pub mod wake;
pub mod workers;

pub use machine::{Machine, MachineConfig, MachineError, StopReason};
