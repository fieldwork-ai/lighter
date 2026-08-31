//! Assembling and running a machine.
//!
//! The construction order here is not stylistic — it is the order the framework
//! and the boot protocol require, and each step is commented with what breaks
//! if it moves.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use lighter_hv::{Gic, GicLayout, Vm};

use crate::bus::MmioBus;
use crate::console::{RawMode, spawn_input_thread};
use crate::devices::pl011::Pl011;
use crate::fdt::{self, FdtParams};
use crate::irq::GicSpi;
use crate::kernel::KernelLoader;
use crate::layout::{GuestLayout, UART_SPI};
use crate::memory::GuestMemory;
use crate::net::Network;
use crate::smp::CpuPark;
use crate::vcpu::{RunContext, StopReason, VcpuRunner};
use crate::virtio::VirtioDevice;
use crate::virtio::balloon::{Balloon, BalloonState};
use crate::virtio::block::Block;
use crate::virtio::disk::Disk;
use crate::virtio::mmio::VirtioMmio;
use crate::virtio::net::Net;
use crate::virtio::rng::Rng;
use crate::virtio::vsock::{Vsock, VsockShared};
use crate::vsock_proxy::VsockProxy;
use crate::{net, virtio};

#[derive(Debug, thiserror::Error)]
pub enum MachineError {
    #[error("hypervisor: {0}")]
    Hv(#[from] lighter_hv::HvError),
    #[error("memory layout: {0}")]
    Layout(#[from] crate::layout::LayoutError),
    #[error("guest memory: {0}")]
    Memory(#[from] crate::memory::MemoryError),
    #[error("kernel: {0}")]
    Kernel(#[from] crate::kernel::KernelError),
    #[error("device tree: {0}")]
    Fdt(#[from] vm_fdt::Error),
    #[error("device bus: {0}")]
    Bus(#[from] crate::bus::BusError),
    #[error("vCPU: {0}")]
    Run(#[from] crate::vcpu::RunError),
    #[error("console: {0}")]
    Console(#[from] io::Error),
    #[error("network: {0}")]
    Network(#[from] crate::net::NetError),
    #[error(
        "this host reports no hardware virtualization support (kern.hv_support = 0); \
         lighter cannot run inside another VM"
    )]
    NoHypervisor,
    #[error(
        "{count} virtio devices requested but the memory map has only {} slots",
        crate::layout::VIRTIO_MMIO_SLOTS
    )]
    TooManyDevices { count: usize },
}

/// How to build the machine.
#[derive(Debug, Clone)]
pub struct MachineConfig {
    pub vcpus: u32,
    pub ram_bytes: u64,
    pub kernel: PathBuf,
    pub initramfs: Option<PathBuf>,
    pub cmdline: String,
    /// Attach the host terminal to the guest console.
    pub interactive: bool,
    /// Disk images, in order. The first becomes /dev/vda.
    pub disks: Vec<PathBuf>,
    /// Logical size for a disk image that has to be created.
    pub disk_size_bytes: u64,
    /// The gvproxy binary to run the network through. `None` builds a machine
    /// with no network device at all, which is what the boot and device gates
    /// want: they are testing something else, and a missing sidecar should not
    /// fail them.
    pub gvproxy: Option<PathBuf>,
    /// Where the network's sockets live.
    pub run_dir: PathBuf,
}

impl Default for MachineConfig {
    fn default() -> Self {
        MachineConfig {
            vcpus: 1,
            ram_bytes: 2 << 30,
            kernel: PathBuf::from("guest/out/Image"),
            initramfs: None,
            // `panic=-1` so a guest that dies exits the VMM instead of sitting
            // at a dead prompt; `reboot=t` makes reboot a PSCI call we see.
            cmdline: "console=ttyAMA0 earlycon=pl011,0xc000000 panic=-1 reboot=t".into(),
            interactive: true,
            disks: Vec::new(),
            disk_size_bytes: 64 << 30,
            gvproxy: None,
            run_dir: std::env::temp_dir().join("lighter"),
        }
    }
}

/// A built, running machine.
pub struct Machine {
    /// Kept alive for the machine's lifetime. Nothing reads it: the VM is
    /// reached through the clones held by guest memory and the vCPU threads,
    /// and `hv_vm_destroy` runs when the last of them goes.
    _vm: Arc<Vm>,
    ctx: Arc<RunContext>,
    threads: Vec<JoinHandle<Result<StopReason, crate::vcpu::RunError>>>,
    /// Held for the machine's lifetime; restores the terminal on drop.
    _raw_mode: Option<RawMode>,
    /// Kept alive because devices hold `Arc<Gic>` clones.
    _gic: Arc<Gic>,
    /// Kept alive because the guest's mappings point into it.
    _memory: Arc<GuestMemory>,
    /// The virtio transports, held so the policy loop can reach them and so
    /// they outlive the guest that is using them.
    virtio: Vec<Arc<Mutex<VirtioMmio>>>,
    disks: Vec<Arc<Disk>>,
    balloon: Arc<BalloonState>,
    /// The gvproxy sidecar, if there is one. Held because dropping it kills the
    /// process, and because port forwards are added through it at runtime.
    network: Option<Network>,
    vsock: Arc<VsockShared>,
    /// Socket proxies, held because dropping one unlinks its socket.
    proxies: Vec<VsockProxy>,
}

impl Machine {
    /// Builds a machine and starts every core.
    pub fn start(config: &MachineConfig) -> Result<Machine, MachineError> {
        if !lighter_hv::hv_supported() {
            return Err(MachineError::NoHypervisor);
        }

        // 1. The VM must exist before anything else can be created for it.
        let vm = Arc::new(Vm::create()?);

        // 2. The GIC comes before the layout, not after. Its windows sit at
        //    fixed addresses that are ours to choose, but its *sizes* are the
        //    host's to report, and those sizes go straight into the device tree
        //    as the region the guest scans for its redistributors. Reading them
        //    from a GIC that exists removes a class of question about whether
        //    they were meaningful yet.
        //
        //    It must also be created before the first vCPU: it allocates
        //    per-core interrupt state and refuses once a vCPU has claimed it.
        //    That is why no vCPU is created until step 8.
        let gic = Arc::new(Gic::create(
            &vm,
            GicLayout {
                distributor_base: GuestLayout::GICD_BASE,
                redistributor_base: GuestLayout::GICR_BASE,
                msi_region_base: None,
                msi_intid_range: (0, 0),
            },
        )?);

        // 3. The rest of the map, derived from the geometry the GIC reported.
        let layout = GuestLayout::new(&gic.params(), config.vcpus, config.ram_bytes)?;
        tracing::debug!(
            gicd = format_args!("{:#x}..{:#x}", layout.gicd.base, layout.gicd.end()),
            gicr = format_args!("{:#x}..{:#x}", layout.gicr.base, layout.gicr.end()),
            "interrupt controller placed"
        );

        // 4. Guest RAM.
        let mut memory = GuestMemory::new(vm.clone());
        memory.add_region(layout.ram.base, layout.ram.size as usize)?;
        let memory = Arc::new(memory);

        // 5. Kernel and initramfs.
        let mut loader = KernelLoader::new(&layout, &config.kernel)?;
        if let Some(initramfs) = &config.initramfs {
            loader = loader.with_initramfs(initramfs)?;
        }
        let (boot, initramfs) = loader.load(&memory)?;

        // 6. Devices, and the bus they sit on.
        let raw_mode = if config.interactive {
            RawMode::enable()?
        } else {
            None
        };
        let uart_irq = Arc::new(GicSpi::new(gic.clone(), UART_SPI)?);
        let uart = Arc::new(Mutex::new(Pl011::new(Box::new(io::stdout()), uart_irq)));
        let mut bus = MmioBus::new();
        bus.register(layout.uart, uart.clone())?;

        // virtio devices occupy consecutive slots, and the device tree must
        // describe exactly the ones that exist: an advertised slot with nothing
        // behind it costs the guest a failed probe, and a populated slot with no
        // node is a device the guest never finds.
        let balloon_state = Arc::new(BalloonState::default());
        let mut virtio: Vec<Box<dyn VirtioDevice>> = Vec::new();
        let mut disks = Vec::new();

        // Networking starts before the device that uses it, so that a missing
        // or unstartable gvproxy is an error about the network rather than a
        // half-built machine. The device's slot index is remembered because the
        // receive pump needs the transport, which does not exist until every
        // device has been placed.
        let network = match &config.gvproxy {
            Some(path) => Some(Network::start(path, &config.run_dir)?),
            None => None,
        };
        let mut net_slot = None;
        let net_inbox = Net::new_inbox();

        for path in &config.disks {
            let disk = Arc::new(Disk::open_or_create(path, config.disk_size_bytes, false)?);
            tracing::info!(
                path = %path.display(),
                capacity_mib = disk.len() / (1 << 20),
                allocated_kib = disk.allocated_bytes().unwrap_or(0) / 1024,
                "attached disk"
            );
            disks.push(disk.clone());
            virtio.push(Box::new(Block::new(disk)));
        }
        if let Some(network) = &network {
            net_slot = Some(virtio.len());
            virtio.push(Box::new(Net::new(
                network.backend(),
                net::GUEST_MAC,
                net_inbox.clone(),
            )));
        }

        // vsock is unconditional. It costs one slot and nothing else when idle,
        // and everything host-to-guest above the network — the Docker socket,
        // the control channel — rides on it.
        let vsock_state = Arc::new(VsockShared::new());
        let vsock_slot = virtio.len();
        virtio.push(Box::new(Vsock::new(vsock_state.clone())));

        virtio.push(Box::new(Rng::from_host()?));
        virtio.push(Box::new(Balloon::new(balloon_state.clone())));

        let virtio_slots = virtio.len();
        let mut virtio_devices = Vec::with_capacity(virtio_slots);
        for (index, device) in virtio.into_iter().enumerate() {
            let window = layout
                .virtio_slot(index)
                .ok_or(MachineError::TooManyDevices {
                    count: virtio_slots,
                })?;
            let irq = Arc::new(GicSpi::new(gic.clone(), layout.virtio_spi(index))?);
            let transport = Arc::new(Mutex::new(VirtioMmio::new(device, memory.clone(), irq)));
            bus.register(window, transport.clone())?;
            virtio_devices.push(transport);
        }

        // vsock queues packets from host threads and must be able to deliver
        // them itself; see the note on the waker in the vsock module.
        {
            let transport = virtio_devices[vsock_slot].clone();
            vsock_state.set_waker(move || {
                transport
                    .lock()
                    .expect("vsock transport poisoned")
                    .service_queue(virtio::vsock::RX_QUEUE);
            });
        }

        // The pump that moves frames off the network and into the guest. It runs
        // outside the vCPU threads because a frame can arrive at any time,
        // including while every core is idle in WFI — which is exactly the
        // moment a guest is waiting for a reply.
        if let (Some(network), Some(slot)) = (&network, net_slot) {
            let transport = virtio_devices[slot].clone();
            network.spawn_receiver(net_inbox, move || {
                transport
                    .lock()
                    .expect("net transport poisoned")
                    .service_queue(virtio::net::RX_QUEUE);
            })?;
        }

        // 7. The device tree describes the machine built above, from the same
        //    layout rather than a parallel description of it.
        let dtb = fdt::build(&FdtParams {
            layout: &layout,
            vcpus: config.vcpus,
            cmdline: &config.cmdline,
            initramfs,
            virtio_slots,
        })?;
        memory.write(boot.dtb, &dtb)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        if config.interactive {
            spawn_input_thread(uart.clone(), Arc::new(AtomicBool::new(true)));
        }

        // 8. One thread per core. Each creates its own vCPU, because
        //    hv_vcpu_create binds to the calling thread.
        let ctx = Arc::new(RunContext {
            bus,
            park: Arc::new(CpuPark::new(config.vcpus)),
            shutdown: shutdown.clone(),
            handles: Mutex::new(Vec::with_capacity(config.vcpus as usize)),
        });

        let mut threads = Vec::with_capacity(config.vcpus as usize);
        for index in 0..config.vcpus {
            let vm = vm.clone();
            let ctx = ctx.clone();
            let handle = std::thread::Builder::new()
                .name(format!("vcpu-{index}"))
                .spawn(move || {
                    // Creation is serialized so that thread index, vCPU id and
                    // MPIDR affinity all agree; see CpuPark::await_creation_turn
                    // for what goes wrong when they do not.
                    ctx.park.await_creation_turn(index);
                    let created =
                        vm.create_vcpu()
                            .map_err(|source| crate::vcpu::RunError::Hypervisor {
                                vcpu: u64::from(index),
                                source,
                            });
                    // Release the next core either way, rather than deadlocking
                    // the whole machine behind a failure on this one.
                    ctx.park.finish_creation(index);
                    let vcpu = created?;

                    // The invariant the ordering exists to guarantee. Checked
                    // here so a violation is named, rather than reaching the
                    // guest as a missing redistributor.
                    if vcpu.id() != u64::from(index) {
                        return Err(crate::vcpu::RunError::VcpuIdMismatch {
                            expected: u64::from(index),
                            actual: vcpu.id(),
                        });
                    }

                    // Publish before running: from here on this core can be
                    // forced out of the guest by whoever stops the machine.
                    ctx.handles
                        .lock()
                        .expect("handle registry poisoned")
                        .push(vcpu.handle());
                    let mut runner = VcpuRunner::new(vcpu, index, ctx);
                    if index == 0 {
                        runner.prepare_boot(boot.entry, boot.dtb)?;
                        runner.run()
                    } else {
                        runner.run_secondary()
                    }
                })
                .expect("failed to spawn vCPU thread");
            threads.push(handle);
        }

        tracing::info!(
            vcpus = config.vcpus,
            ram_mib = config.ram_bytes / (1 << 20),
            entry = format_args!("{:#x}", boot.entry),
            dtb = format_args!("{:#x}", boot.dtb),
            "machine started"
        );

        Ok(Machine {
            _vm: vm,
            ctx,
            threads,
            _raw_mode: raw_mode,
            _gic: gic,
            _memory: memory,
            virtio: virtio_devices,
            disks,
            balloon: balloon_state,
            network,
            vsock: vsock_state,
            proxies: Vec::new(),
        })
    }

    /// The guest's network, if one was configured.
    pub fn network(&self) -> Option<&Network> {
        self.network.as_ref()
    }

    /// Proxies a host unix socket to a port inside the guest.
    ///
    /// Kept on the machine rather than returned, because the proxy unlinks its
    /// socket when dropped and a caller that let it fall out of scope would get
    /// a path that briefly worked.
    pub fn proxy_socket(&mut self, path: &std::path::Path, guest_port: u32) -> io::Result<()> {
        let proxy = VsockProxy::listen(path, guest_port, self.vsock.clone())?;
        self.proxies.push(proxy);
        Ok(())
    }

    /// Waits for the guest to stop, returning why.
    pub fn wait(mut self) -> Result<StopReason, MachineError> {
        // The boot core decides the machine's fate; secondaries follow it down.
        let primary = self.threads.remove(0);
        let reason = primary.join().unwrap_or(Ok(StopReason::Shutdown))?;

        self.stop_others();
        Ok(reason)
    }

    /// The balloon's shared state, for the memory policy loop.
    pub fn balloon(&self) -> &Arc<BalloonState> {
        &self.balloon
    }

    /// The attached disks, for reporting host allocation.
    pub fn disks(&self) -> &[Arc<Disk>] {
        &self.disks
    }

    /// The virtio transports, for host-driven queue servicing.
    pub fn virtio(&self) -> &[Arc<Mutex<VirtioMmio>>] {
        &self.virtio
    }

    /// Asks every core to stop.
    pub fn shutdown(&self) {
        self.ctx.stop();
    }

    fn stop_others(&mut self) {
        self.shutdown();
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        // Destroying the VM while a vCPU thread is still executing is
        // undefined, so every core is stopped and joined first.
        self.stop_others();
        debug_assert!(
            self.threads.is_empty(),
            "a vCPU thread was still running when the machine was dropped"
        );

        // The VM itself outliving its guest mappings is handled by `Arc`
        // rather than by drop order: `GuestMemory` holds its own clone and
        // unmaps each region on the way out, so `hv_vm_destroy` cannot run
        // until that has happened. Counting clones here would just encode how
        // many things legitimately hold one.
    }
}
