//! The virtual machine, as Virtualization.framework runs it.
//!
//! Everything Objective-C in the crate is in this file. The framework wants
//! every call on the dispatch queue the machine was created with, so the
//! objects live here behind [`Vm`] and every method hops onto that queue;
//! nothing else holds a framework object.
//!
//! What the framework provides: the vCPUs, memory, interrupt controller and
//! timers, a virtio console, block devices, a balloon, an entropy device, a
//! network card whose far end is a file descriptor we own, and the Rosetta
//! directory share. What it does not provide is anything we can put code
//! behind, which is why the file server and the network live on the card's
//! socket rather than in a device.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{
    NSArray, NSError, NSFileHandle, NSObject, NSObjectProtocol, NSString, NSURL,
};
use objc2_virtualization::*;

/// A framework object is bound to a queue, not a thread: holding it anywhere
/// is fine as long as every call goes through [`on`].
struct Anywhere<T>(T);
// SAFETY: see above; the wrapped values are only ever used inside `on`, on
// the machine's queue.
unsafe impl<T> Send for Anywhere<T> {}
unsafe impl<T> Sync for Anywhere<T> {}

impl<T> Anywhere<T> {
    fn into_inner(self) -> T {
        self.0
    }
}

/// Runs `f` on the queue and waits for it.
fn on<R>(queue: &DispatchQueue, f: impl FnOnce() -> R) -> R {
    let f = Anywhere(f);
    let slot: Arc<Mutex<Option<Anywhere<R>>>> = Arc::new(Mutex::new(None));
    let out = slot.clone();
    queue.exec_sync(move || {
        let f = f.into_inner();
        *out.lock().expect("queue slot poisoned") = Some(Anywhere(f()));
    });
    let taken = slot
        .lock()
        .expect("queue slot poisoned")
        .take()
        .expect("the queue ran the block");
    taken.into_inner()
}

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn url(p: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&ns(&p.to_string_lossy()))
}

fn describe(e: &NSError) -> String {
    e.localizedDescription().to_string()
}

/// What the machine reports back, in the order it happened.
#[derive(Debug)]
pub enum Event {
    /// The guest powered itself off.
    GuestStopped,
    /// The framework stopped the machine.
    StoppedWithError(String),
}

struct DelegateIvars {
    events: Mutex<mpsc::Sender<Event>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "LighterMachineDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl VZVirtualMachineDelegate for Delegate {
        #[unsafe(method(guestDidStopVirtualMachine:))]
        fn guest_did_stop(&self, _vm: &VZVirtualMachine) {
            let _ = self
                .ivars()
                .events
                .lock()
                .expect("delegate poisoned")
                .send(Event::GuestStopped);
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn did_stop_with_error(&self, _vm: &VZVirtualMachine, error: &NSError) {
            let _ = self
                .ivars()
                .events
                .lock()
                .expect("delegate poisoned")
                .send(Event::StoppedWithError(describe(error)));
        }
    }
);

impl Delegate {
    fn new(events: mpsc::Sender<Event>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars {
            events: Mutex::new(events),
        });
        // SAFETY: NSObject's init on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Whether the Mac can run Rosetta for the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rosetta {
    Installed,
    NotInstalled,
    NotSupported,
}

/// Asks the framework.
pub fn rosetta() -> Rosetta {
    // SAFETY: a class property read.
    match unsafe { VZLinuxRosettaDirectoryShare::availability() } {
        VZLinuxRosettaAvailability::Installed => Rosetta::Installed,
        VZLinuxRosettaAvailability::NotInstalled => Rosetta::NotInstalled,
        _ => Rosetta::NotSupported,
    }
}

/// Runs Apple's installer prompt; returns when it finished or was refused.
pub fn install_rosetta() -> Result<(), String> {
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let block = RcBlock::new(move |e: *mut NSError| {
        // SAFETY: a nullable NSError pointer from the framework.
        let r = if e.is_null() {
            Ok(())
        } else {
            Err(describe(unsafe { &*e }))
        };
        let _ = tx.send(r);
    });
    // SAFETY: a class method; the block outlives the call because the
    // framework copies it.
    unsafe { VZLinuxRosettaDirectoryShare::installRosettaWithCompletionHandler(&block) };
    rx.recv()
        .map_err(|_| "the installer never answered".to_string())?
}

/// How the guest's console reaches us.
pub struct Console {
    /// Where the guest's output goes.
    pub output: OwnedFd,
    /// Where the guest's input comes from, if anywhere.
    pub input: Option<OwnedFd>,
}

/// What to build.
pub struct Config {
    pub cpus: usize,
    pub memory_bytes: u64,
    pub kernel: PathBuf,
    pub initramfs: Option<PathBuf>,
    pub cmdline: String,
    pub console: Console,
    pub disks: Vec<PathBuf>,
    /// The machine's end of the card, and the MTU it advertises.
    pub card: Option<(OwnedFd, u16)>,
    pub mac: [u8; 6],
    pub rosetta: bool,
}

/// A running machine.
pub struct Vm {
    queue: DispatchRetained<DispatchQueue>,
    vm: Anywhere<Retained<VZVirtualMachine>>,
    _delegate: Anywhere<Retained<Delegate>>,
    balloon: Option<Anywhere<Retained<VZVirtioTraditionalMemoryBalloonDevice>>>,
    events: Mutex<mpsc::Receiver<Event>>,
    memory_bytes: u64,
    helper: Option<i32>,
}

const HELPER_NAME: &str = "com.apple.Virtualization.VirtualMachine";

unsafe extern "C" {
    fn proc_listpids(
        kind: u32,
        typeinfo: u32,
        buffer: *mut libc::c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
    fn proc_name(pid: libc::c_int, buffer: *mut libc::c_void, buffersize: u32) -> libc::c_int;
}

/// Every framework helper process on the Mac. The framework runs each
/// machine in an XPC service launchd starts, so the helper is nobody's
/// child; the one that appears while a machine starts is that machine's.
fn helper_pids() -> Vec<i32> {
    const PROC_ALL_PIDS: u32 = 1;
    let mut pids = vec![0 as libc::c_int; 4096];
    // SAFETY: the buffer is ours and its size is passed in bytes.
    let bytes = unsafe {
        proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int,
        )
    };
    let count = (bytes.max(0) as usize) / std::mem::size_of::<libc::c_int>();
    let mut out = Vec::new();
    for &pid in pids.iter().take(count) {
        if pid <= 0 {
            continue;
        }
        let mut name = [0u8; 256];
        // SAFETY: proc_name writes at most `len` bytes into our buffer.
        let n = unsafe { proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) };
        // The kernel keeps a truncated name (32 bytes), so a prefix match.
        let got = &name[..n.max(0) as usize];
        let want = HELPER_NAME.as_bytes();
        let len = got.len().min(want.len());
        if len >= 16 && got[..len] == want[..len] {
            out.push(pid);
        }
    }
    out
}

impl Vm {
    /// Builds, validates and starts the machine; returns once the guest is
    /// running (not booted).
    pub fn start(config: Config) -> Result<Vm, String> {
        // SAFETY: framework calls with valid arguments, all before the
        // machine exists and therefore on no queue in particular.
        unsafe {
            let vzc = VZVirtualMachineConfiguration::new();
            vzc.setCPUCount(config.cpus);
            vzc.setMemorySize(config.memory_bytes);
            vzc.setPlatform(&VZGenericPlatformConfiguration::new());

            let boot = VZLinuxBootLoader::initWithKernelURL(
                VZLinuxBootLoader::alloc(),
                &url(&config.kernel),
            );
            boot.setCommandLine(&ns(&config.cmdline));
            if let Some(initramfs) = &config.initramfs {
                boot.setInitialRamdiskURL(Some(&url(initramfs)));
            }
            vzc.setBootLoader(Some(&boot));

            // The console: a virtio console port on file handles we own.
            // `closeOnDealloc` is false because the fds are ours to close.
            let out = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                NSFileHandle::alloc(),
                config.console.output.as_raw_fd(),
                false,
            );
            let input = config.console.input.as_ref().map(|fd| {
                NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                    NSFileHandle::alloc(),
                    fd.as_raw_fd(),
                    false,
                )
            });
            let attachment =
                VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                    VZFileHandleSerialPortAttachment::alloc(),
                    input.as_deref(),
                    Some(&out),
                );
            let port = VZVirtioConsoleDeviceSerialPortConfiguration::new();
            port.setAttachment(Some(&attachment));
            vzc.setSerialPorts(&NSArray::from_retained_slice(&[Retained::into_super(port)]));

            // Disks: virtio-blk over the image file, the host's cache on
            // (S4: reads at 3 µs against 98 from the SSD), flushes honoured
            // with fsync so a guest flush is durable.
            let mut storage: Vec<Retained<VZStorageDeviceConfiguration>> = Vec::new();
            for disk in &config.disks {
                let attachment =
                    VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
                        VZDiskImageStorageDeviceAttachment::alloc(),
                        &url(disk),
                        false,
                        VZDiskImageCachingMode::Cached,
                        VZDiskImageSynchronizationMode::Fsync,
                    )
                    .map_err(|e| format!("disk {}: {}", disk.display(), describe(&e)))?;
                storage.push(Retained::into_super(
                    VZVirtioBlockDeviceConfiguration::initWithAttachment(
                        VZVirtioBlockDeviceConfiguration::alloc(),
                        &attachment,
                    ),
                ));
            }
            vzc.setStorageDevices(&NSArray::from_retained_slice(&storage));

            vzc.setMemoryBalloonDevices(&NSArray::from_retained_slice(&[Retained::into_super(
                VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new(),
            )]));
            vzc.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(
                VZVirtioEntropyDeviceConfiguration::new(),
            )]));

            if let Some((fd, mtu)) = &config.card {
                let handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                    NSFileHandle::alloc(),
                    fd.as_raw_fd(),
                    false,
                );
                let attachment = VZFileHandleNetworkDeviceAttachment::initWithFileHandle(
                    VZFileHandleNetworkDeviceAttachment::alloc(),
                    &handle,
                );
                attachment.setMaximumTransmissionUnit(*mtu as isize);
                let card = VZVirtioNetworkDeviceConfiguration::new();
                card.setAttachment(Some(&attachment));
                let m = config.mac;
                let text = format!(
                    "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    m[0], m[1], m[2], m[3], m[4], m[5]
                );
                let mac = VZMACAddress::initWithString(VZMACAddress::alloc(), &ns(&text))
                    .ok_or_else(|| format!("bad MAC address {text}"))?;
                card.setMACAddress(&mac);
                vzc.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(card)]));
            }

            if config.rosetta {
                let share = VZLinuxRosettaDirectoryShare::initWithError(
                    VZLinuxRosettaDirectoryShare::alloc(),
                )
                .map_err(|e| format!("rosetta share: {}", describe(&e)))?;
                let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                    VZVirtioFileSystemDeviceConfiguration::alloc(),
                    &ns("rosetta"),
                );
                fs.setShare(Some(&share));
                vzc.setDirectorySharingDevices(&NSArray::from_retained_slice(&[
                    Retained::into_super(fs),
                ]));
            }

            vzc.validateWithError()
                .map_err(|e| format!("machine configuration: {}", describe(&e)))?;

            let queue = DispatchQueue::new("lighter.machine", None);
            let vm = VZVirtualMachine::initWithConfiguration_queue(
                VZVirtualMachine::alloc(),
                &vzc,
                &queue,
            );
            let (events_tx, events_rx) = mpsc::channel();
            let delegate = Delegate::new(events_tx);
            let vm = Anywhere(vm);
            let delegate = Anywhere(delegate);
            on(&queue, || {
                vm.0.setDelegate(Some(ProtocolObject::from_ref(&*delegate.0)))
            });

            let helpers_before = helper_pids();
            let (started_tx, started_rx) = mpsc::channel::<Result<(), String>>();
            on(&queue, || {
                let block = RcBlock::new(move |e: *mut NSError| {
                    let _ = started_tx.send(if e.is_null() {
                        Ok(())
                    } else {
                        Err(describe(&*e))
                    });
                });
                vm.0.startWithCompletionHandler(&block);
            });
            started_rx
                .recv()
                .map_err(|_| "the start handler never ran".to_string())??;

            let balloon = on(&queue, || {
                vm.0.memoryBalloonDevices().firstObject().map(|d| {
                    Anywhere(Retained::cast_unchecked::<
                        VZVirtioTraditionalMemoryBalloonDevice,
                    >(d))
                })
            });
            // The helper that was not there before the start is this
            // machine's; it can take a moment to register.
            let mut helper = None;
            for _ in 0..40 {
                let new: Vec<i32> = helper_pids()
                    .into_iter()
                    .filter(|p| !helpers_before.contains(p))
                    .collect();
                if let Some(&pid) = new.iter().max() {
                    helper = Some(pid);
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if let Some(pid) = helper {
                crate::footprint::set_helper(pid);
                tracing::debug!(pid, "the framework's helper process");
            } else {
                tracing::warn!(
                    "cannot find the framework's helper process; footprints will miss the guest"
                );
            }

            Ok(Vm {
                queue,
                vm,
                _delegate: delegate,
                balloon,
                events: Mutex::new(events_rx),
                memory_bytes: config.memory_bytes,
                helper,
            })
        }
    }

    /// Waits for the next event, or `None` after `timeout`.
    pub fn next_event(&self, timeout: Duration) -> Option<Event> {
        self.events
            .lock()
            .expect("events poisoned")
            .recv_timeout(timeout)
            .ok()
    }

    /// Stops the machine without asking the guest.
    pub fn stop(&self) -> Result<(), String> {
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let vm = &self.vm;
        on(&self.queue, || {
            let block = RcBlock::new(move |e: *mut NSError| {
                // SAFETY: nullable NSError from the framework.
                let _ = tx.send(if e.is_null() {
                    Ok(())
                } else {
                    Err(describe(unsafe { &*e }))
                });
            });
            // SAFETY: on the machine's queue.
            unsafe { vm.0.stopWithCompletionHandler(&block) };
        });
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|_| "stop timed out".to_string())?
    }

    /// Tells the balloon how much of the guest's memory the guest may keep;
    /// the rest it hands back.
    pub fn set_guest_memory(&self, bytes: u64) {
        let Some(balloon) = &self.balloon else { return };
        let bytes = bytes.min(self.memory_bytes);
        // SAFETY: on the machine's queue.
        on(&self.queue, || unsafe {
            balloon.0.setTargetVirtualMachineMemorySize(bytes)
        });
    }

    /// What the balloon target is now.
    pub fn guest_memory(&self) -> u64 {
        let Some(balloon) = &self.balloon else {
            return self.memory_bytes;
        };
        // SAFETY: on the machine's queue.
        on(&self.queue, || unsafe {
            balloon.0.targetVirtualMachineMemorySize()
        })
    }

    /// The framework's helper process for this machine, where the guest's
    /// memory is charged.
    pub fn helper_pid(&self) -> Option<i32> {
        self.helper
    }

    /// Whether the framework can run a machine at all.
    pub fn supported() -> bool {
        // SAFETY: a class method.
        unsafe { VZVirtualMachine::isSupported() }
    }
}
