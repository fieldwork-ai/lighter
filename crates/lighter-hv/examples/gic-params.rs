//! Prints the host's GIC geometry before and after the GIC is created.
//!
//! Written to settle a specific question: `Machine::start` queried these
//! parameters before calling `hv_gic_create`, and a guest once booted with
//! "GICv3: No redistributor present", which is what a device tree built from
//! wrong sizes looks like from inside.

use lighter_hv::{Gic, GicLayout, GicParameters, Vm};

fn show(label: &str, p: &GicParameters) {
    println!("{label}:");
    println!("  distributor_size          {:#x}", p.distributor_size);
    println!("  distributor_alignment     {:#x}", p.distributor_alignment);
    println!("  redistributor_size        {:#x}", p.redistributor_size);
    println!(
        "  redistributor_region_size {:#x}",
        p.redistributor_region_size
    );
    println!(
        "  redistributor_alignment   {:#x}",
        p.redistributor_alignment
    );
    println!(
        "  spi range                 {}..{}",
        p.spi_base,
        p.spi_base + p.spi_count
    );
}

fn main() {
    let vm = Vm::create().expect("create VM");

    match GicParameters::query() {
        Ok(p) => show("before hv_gic_create", &p),
        Err(e) => println!("before hv_gic_create: query failed: {e}"),
    }

    let gic = Gic::create(&vm, GicLayout::default()).expect("create GIC");
    show("after hv_gic_create", &gic.params());

    // The number that actually matters: where each core's redistributor lands,
    // and therefore how large the device tree's region must be.
    println!("\nredistributor bases:");
    let mut previous = None;
    for i in 0..4 {
        let vcpu = vm.create_vcpu().expect("create vCPU");
        // Not every host answers this: it returns HV_BAD_ARGUMENT on macOS 26
        // even for a freshly created vCPU. Report that rather than dying, since
        // this program exists to tell you what the host does.
        match gic.redistributor_base(vcpu.id()) {
            Ok(base) => {
                match previous.map(|p: u64| base - p) {
                    Some(s) => println!("  vcpu {i}: {base:#x} (stride {s:#x})"),
                    None => println!("  vcpu {i}: {base:#x}"),
                }
                previous = Some(base);
            }
            Err(e) => println!("  vcpu {i}: unavailable ({e})"),
        }
        std::mem::forget(vcpu);
    }
}
