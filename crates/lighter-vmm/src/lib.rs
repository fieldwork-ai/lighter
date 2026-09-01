//! The lighter machine model.
//!
//! # The secret this crate keeps
//!
//! What the guest's machine *is*: where memory and devices sit, how a kernel is
//! loaded, what the device tree says, how a core is started and stopped, and
//! how a device access is serviced. It knows there is a hypervisor underneath
//! only through [`lighter_hv`]'s interface.
//!
//! # Layering
//!
//! ```text
//!   machine   assembles everything in the one legal order
//!     ├── layout    where things are, derived once and shared
//!     ├── memory    guest RAM, as raw shared memory rather than slices
//!     ├── kernel    the arm64 boot protocol
//!     ├── fdt       the machine description handed to the kernel
//!     ├── bus       MMIO dispatch
//!     ├── vcpu      the run loop
//!     ├── smp       PSCI core power state
//!     ├── sysreg    trapped system-register policy
//!     ├── devices   models that answer MMIO
//!     ├── virtio    the transport and the device models on it, file sharing
//!     │             among them
//!     └── net       the host end of the network, behind a process boundary
//! ```

pub mod bus;
pub mod console;
pub mod devices;
pub mod fdt;
pub mod footprint;
pub mod irq;
pub mod kernel;
pub mod layout;
pub mod machine;
pub mod memory;
pub mod memory_policy;
pub mod mempressure;
pub mod net;
pub mod psci;
pub mod smp;
pub mod sysreg;
pub mod vcpu;
pub mod virtio;
pub mod vsock_proxy;
pub mod wake;

pub use machine::{Machine, MachineConfig, MachineError};
pub use vcpu::StopReason;
