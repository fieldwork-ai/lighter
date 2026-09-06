//! Small Hypervisor.framework accounting reproduction; no Linux guest needed.
//!
//! Build with `cargo build --release -p lighter-vmm --example guest-host-accounting`,
//! then sign with `scripts/sign.sh target/release/examples/guest-host-accounting`.
//! Prints the task footprint after guest writes, host reads, partial release,
//! and reuse. The values are observations, not fixed expected accounting.

use std::sync::Arc;

use lighter_hv::{Exception, Exit, Gic, GicLayout, Reg, Vcpu, Vm};
use lighter_vmm::{footprint, memory::GuestMemory};

const CODE: u64 = 0x4000_0000;
const DATA: u64 = 0x8000_0000;
const PAGE: usize = 16 * 1024;
const SIZE: usize = 64 * 1024 * 1024;

fn run(cpu: &mut Vcpu, pc: u64) {
    cpu.set_reg(Reg::Pc, pc).unwrap();
    assert!(matches!(cpu.run().unwrap(), Exit::Exception(e) if e.class() == Exception::EC_BRK64));
}

fn fill(cpu: &mut Vcpu, value: u64) {
    cpu.set_reg(Reg::X0, DATA).unwrap();
    cpu.set_reg(Reg::X1, DATA + SIZE as u64).unwrap();
    cpu.set_reg(Reg::X2, value).unwrap();
    run(cpu, CODE);
}

fn check(memory: &GuestMemory, offset: usize, len: usize, value: u64) {
    for at in (offset..offset + len).step_by(PAGE) {
        assert_eq!(memory.read_u64(DATA + at as u64).unwrap(), value);
    }
}

fn measure(stage: &str) {
    println!("{stage},{}", footprint::bytes());
}

fn main() {
    let vm = Arc::new(Vm::create().unwrap());
    let _gic = Gic::create(&vm, GicLayout::default()).unwrap();
    let mut memory = GuestMemory::new(vm.clone());
    memory.add_region(CODE, PAGE).unwrap();
    memory.add_region(DATA, SIZE).unwrap();
    // str x2,[x0]; add x0,x0,#4096; cmp x0,x1; b.lo loop; brk #0.
    for (i, word) in [
        0xf900_0002_u32,
        0x9140_0400,
        0xeb01_001f,
        0x54ff_ffa3,
        0xd420_0000,
    ]
    .into_iter()
    .enumerate()
    {
        memory
            .write(CODE + (i * 4) as u64, &word.to_le_bytes())
            .unwrap();
    }
    // ldr x3,[x0]; brk #0: check host writes through the guest mapping.
    memory
        .write(CODE + 64, &0xf940_0003_u32.to_le_bytes())
        .unwrap();
    memory
        .write(CODE + 68, &0xd420_0000_u32.to_le_bytes())
        .unwrap();
    let mut cpu = vm.create_vcpu().unwrap();
    cpu.set_trap_debug_exceptions(true).unwrap();
    cpu.set_reg(Reg::Cpsr, lighter_hv::PSTATE_EL1H_DAIF_MASKED)
        .unwrap();

    println!("stage,phys_footprint_bytes");
    measure("baseline");
    fill(&mut cpu, 1);
    measure("guest_wrote_64_mib");
    check(&memory, 0, SIZE, 1);
    measure("host_read_same_pages");
    memory.write(DATA, &7_u64.to_le_bytes()).unwrap();
    cpu.set_reg(Reg::X0, DATA).unwrap();
    run(&mut cpu, CODE + 64);
    assert_eq!(cpu.reg(Reg::X3).unwrap(), 7);
    measure("host_write_visible_to_guest");

    // The guest is parked. Only the explicitly surrendered first half is
    // released; the second half must survive with its original contents.
    assert_eq!(
        memory.release(DATA, (SIZE / 2) as u64).unwrap(),
        (SIZE / 2) as u64
    );
    check(&memory, SIZE / 2, SIZE / 2, 1);
    measure("released_half");
    fill(&mut cpu, 2);
    measure("guest_reused_half");
    check(&memory, 0, SIZE, 2);
    measure("host_read_reused_pages");
    drop(cpu);
    drop(memory);
    measure("released_all");
}
