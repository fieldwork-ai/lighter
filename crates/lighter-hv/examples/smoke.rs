//! Proves the hypervisor path end to end: entitlement, VM, guest-physical
//! mapping, GICv3, a vCPU, and real guest instructions executing on the core.
//!
//! This is deliberately the smallest program that can fail for each of the
//! reasons a first run fails, and it says which one it was. Run it with
//! `make smoke`.

use std::ffi::c_void;

use lighter_hv::{Exception, Exit, Gic, GicLayout, MemoryPerms, Reg, Vm, hv_supported};

/// Where the test's instructions live in guest-physical space. Arbitrary, but
/// clear of the GIC windows at 0x0800_0000.
const CODE_BASE: u64 = 0x4000_0000;
const PAGE: usize = 16 * 1024;

/// ```text
/// movz x0, #0x42     ; a value the host can recognise
/// brk  #0            ; trap to the VMM
/// ```
const GUEST_CODE: [u32; 2] = [0xd280_0840, 0xd420_0000];

fn main() {
    if !hv_supported() {
        eprintln!("FAIL: kern.hv_support is 0 — no hardware virtualization here.");
        eprintln!("      (This is what you see inside a VM, including hosted CI.)");
        std::process::exit(1);
    }
    println!("ok   host supports hardware virtualization");

    let vm = match Vm::create() {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("FAIL: could not create the VM: {e}");
            std::process::exit(1);
        }
    };
    println!("ok   created VM (max vCPUs: {})", vm.max_vcpu_count().unwrap());

    // The GIC must exist before any vCPU does. Creating it here, before
    // create_vcpu below, is the ordering the whole boot path depends on.
    let gic = match Gic::create(&vm, GicLayout::default()) {
        Ok(gic) => gic,
        Err(e) => {
            eprintln!("FAIL: could not create the GICv3: {e}");
            eprintln!("      hv_gic_* needs macOS 15 or newer.");
            std::process::exit(1);
        }
    };
    let p = gic.params();
    println!(
        "ok   created GICv3 (GICD {} KiB @ {:#x}, GICR region {} KiB @ {:#x}, SPIs {}..{})",
        p.distributor_size / 1024,
        gic.layout().distributor_base,
        p.redistributor_region_size / 1024,
        gic.layout().redistributor_base,
        p.spi_base,
        p.spi_base + p.spi_count,
    );

    // One page of guest RAM holding the instructions.
    // SAFETY: a fresh anonymous mapping; the pointer is checked below and the
    // allocation outlives the VM because it is never unmapped in this process.
    let mem = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    if mem == libc::MAP_FAILED {
        eprintln!("FAIL: mmap: {}", std::io::Error::last_os_error());
        std::process::exit(1);
    }

    // SAFETY: `mem` is a valid PAGE-sized writable allocation and no guest is
    // running yet, so nothing else can observe these bytes mid-write.
    unsafe {
        std::ptr::copy_nonoverlapping(
            GUEST_CODE.as_ptr(),
            mem.cast::<u32>(),
            GUEST_CODE.len(),
        );
    }

    // SAFETY: the mapping stays valid for the rest of the process, which
    // outlives the VM.
    if let Err(e) = unsafe { vm.map(mem as *mut c_void, CODE_BASE, PAGE, MemoryPerms::RWX) } {
        eprintln!("FAIL: could not map guest memory: {e}");
        std::process::exit(1);
    }
    println!("ok   mapped {} KiB of guest RAM at {CODE_BASE:#x}", PAGE / 1024);

    let mut vcpu = match vm.create_vcpu() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("FAIL: could not create a vCPU: {e}");
            std::process::exit(1);
        }
    };
    println!("ok   created vCPU {}", vcpu.id());

    // BRK is a debug exception; without this it goes to the guest, which has no
    // vector table, and the machine spins in a fault loop instead of exiting.
    vcpu.set_trap_debug_exceptions(true).unwrap();
    vcpu.set_reg(Reg::Pc, CODE_BASE).unwrap();
    vcpu.set_reg(Reg::Cpsr, lighter_hv::PSTATE_EL1H_DAIF_MASKED)
        .unwrap();

    match vcpu.run() {
        Ok(Exit::Exception(exc)) if exc.class() == Exception::EC_BRK64 => {
            let x0 = vcpu.reg(Reg::X0).unwrap();
            if x0 == 0x42 {
                println!("ok   guest executed: x0 == {x0:#x}, trapped on BRK");
            } else {
                eprintln!("FAIL: guest ran but x0 == {x0:#x}, expected 0x42");
                std::process::exit(1);
            }
        }
        Ok(other) => {
            eprintln!("FAIL: unexpected exit: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("FAIL: hv_vcpu_run: {e}");
            std::process::exit(1);
        }
    }

    let ns = vcpu.exec_time().unwrap();
    println!("ok   vCPU consumed {ns} ns of guest execution time");
    println!("\nPASS: the hypervisor path works on this machine.");
}
