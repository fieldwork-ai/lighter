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

use lighter_hv::{Gic, GicLayout, GicParameters, Vm};

use crate::bus::MmioBus;
use crate::console::{RawMode, spawn_input_thread};
use crate::devices::pl011::Pl011;
use crate::fdt::{self, FdtParams};
use crate::irq::GicSpi;
use crate::kernel::KernelLoader;
use crate::layout::{GuestLayout, UART_SPI};
use crate::memory::GuestMemory;
use crate::smp::CpuPark;
use crate::vcpu::{RunContext, StopReason, VcpuRunner};

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
    #[error(
        "this host reports no hardware virtualization support (kern.hv_support = 0); \
         lighter cannot run inside another VM"
    )]
    NoHypervisor,
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
        }
    }
}

/// A built, running machine.
pub struct Machine {
    vm: Arc<Vm>,
    ctx: Arc<RunContext>,
    threads: Vec<JoinHandle<Result<StopReason, crate::vcpu::RunError>>>,
    /// Held for the machine's lifetime; restores the terminal on drop.
    _raw_mode: Option<RawMode>,
    /// Kept alive because devices hold `Arc<Gic>` clones.
    _gic: Arc<Gic>,
    /// Kept alive because the guest's mappings point into it.
    _memory: Arc<GuestMemory>,
}

impl Machine {
    /// Builds a machine and starts every core.
    pub fn start(config: &MachineConfig) -> Result<Machine, MachineError> {
        if !lighter_hv::hv_supported() {
            return Err(MachineError::NoHypervisor);
        }

        // 1. The VM must exist before anything else can be created for it.
        let vm = Arc::new(Vm::create()?);

        // 2. The layout depends on the host's GIC geometry, so query before
        //    placing anything.
        let gic_params = GicParameters::query()?;
        let layout = GuestLayout::new(&gic_params, config.vcpus, config.ram_bytes)?;

        // 3. The GIC must be created after the VM and *before the first vCPU* —
        //    it allocates per-core interrupt state and refuses once a vCPU
        //    exists. This is why no vCPU is created until step 8.
        let gic = Arc::new(Gic::create(
            &vm,
            GicLayout {
                distributor_base: layout.gicd.base,
                redistributor_base: layout.gicr.base,
                msi_region_base: None,
                msi_intid_range: (0, 0),
            },
        )?);

        // 4. Guest RAM.
        let mut memory = GuestMemory::new();
        memory.add_region(&vm, layout.ram.base, layout.ram.size as usize)?;
        let memory = Arc::new(memory);

        // 5. Kernel and initramfs.
        let mut loader = KernelLoader::new(&layout, &config.kernel)?;
        if let Some(initramfs) = &config.initramfs {
            loader = loader.with_initramfs(initramfs)?;
        }
        let (boot, initramfs) = loader.load(&memory)?;

        // 6. The device tree describes what step 7 is about to build, so it is
        //    written from the same layout rather than a parallel description.
        let dtb = fdt::build(&FdtParams {
            layout: &layout,
            vcpus: config.vcpus,
            cmdline: &config.cmdline,
            initramfs,
            virtio_slots: 0,
        })?;
        memory.write(boot.dtb, &dtb)?;

        // 7. Devices.
        let raw_mode = if config.interactive {
            RawMode::enable()?
        } else {
            None
        };
        let uart_irq = Arc::new(GicSpi::new(gic.clone(), UART_SPI)?);
        let uart = Arc::new(Mutex::new(Pl011::new(Box::new(io::stdout()), uart_irq)));
        let mut bus = MmioBus::new();
        bus.register(layout.uart, uart.clone())?;

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
                    let vcpu =
                        vm.create_vcpu()
                            .map_err(|source| crate::vcpu::RunError::Hypervisor {
                                vcpu: u64::from(index),
                                source,
                            })?;
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
            vm,
            ctx,
            threads,
            _raw_mode: raw_mode,
            _gic: gic,
            _memory: memory,
        })
    }

    /// Waits for the guest to stop, returning why.
    pub fn wait(mut self) -> Result<StopReason, MachineError> {
        // The boot core decides the machine's fate; secondaries follow it down.
        let primary = self.threads.remove(0);
        let reason = primary.join().unwrap_or(Ok(StopReason::Shutdown))?;

        self.stop_others();
        Ok(reason)
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
        // undefined, so threads are joined before `vm` can be dropped.
        self.stop_others();
        debug_assert_eq!(
            Arc::strong_count(&self.vm),
            1,
            "vCPU threads outlived the VM"
        );
    }
}
