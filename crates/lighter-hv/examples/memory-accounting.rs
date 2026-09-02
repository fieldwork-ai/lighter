//! Measures what macOS charges a process for guest memory, per backing
//! allocation. One VM, one GiB mapped from each kind of host allocation in
//! turn, every page stored to by a tiny guest loop, and the process's
//! footprint read after each. Run signed: `make sign` after building.

use std::process::Command;

use lighter_hv::{Exception, Exit, Gic, GicLayout, MemoryPerms, Reg, Vm};

const CODE_BASE: u64 = 0x4000_0000;
const DATA_BASE: u64 = 0x8000_0000;
const PAGE: usize = 16 * 1024;
const SIZE: usize = 1 << 30;

/// ```text
/// loop: str x0, [x0]
///       add x0, x0, #4096
///       cmp x0, x1
///       b.lo loop
///       brk #0
/// ```
const GUEST_CODE: [u32; 5] = [
    0xf900_0000,
    0x9140_0400,
    0xeb01_001f,
    0x54ff_ffa3,
    0xd420_0000,
];

const VM_FLAGS_ANYWHERE: i32 = 0x0001;
const MAP_MEM_NAMED_CREATE: i32 = 0x0002_0000;
const VM_PROT_RW: i32 = 3;
const VM_INHERIT_NONE: u32 = 2;

unsafe extern "C" {
    fn mach_vm_allocate(target: u32, address: *mut u64, size: u64, flags: i32) -> i32;
    fn mach_vm_deallocate(target: u32, address: u64, size: u64) -> i32;
    fn mach_make_memory_entry_64(
        target: u32,
        size: *mut u64,
        offset: u64,
        permission: i32,
        handle: *mut u32,
        parent: u32,
    ) -> i32;
    fn mach_vm_map(
        target: u32,
        address: *mut u64,
        size: u64,
        mask: u64,
        flags: i32,
        object: u32,
        offset: u64,
        copy: i32,
        cur_prot: i32,
        max_prot: i32,
        inheritance: u32,
    ) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
}

enum Backing {
    Mmap(i32),
    MachAllocate(i32),
    MemoryEntry,
    Shm,
    File,
}

struct Region {
    ptr: *mut u8,
    free: Box<dyn FnOnce(*mut u8)>,
}

fn allocate(backing: &Backing) -> Region {
    #[allow(deprecated)]
    let task = unsafe { libc::mach_task_self() };
    match backing {
        Backing::Mmap(flags) => {
            let p = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_ANON | flags,
                    -1,
                    0,
                )
            };
            assert!(
                p != libc::MAP_FAILED,
                "mmap: {}",
                std::io::Error::last_os_error()
            );
            Region {
                ptr: p.cast(),
                free: Box::new(|p| unsafe {
                    libc::munmap(p.cast(), SIZE);
                }),
            }
        }
        Backing::MachAllocate(flags) => {
            let mut addr = 0u64;
            let rc = unsafe {
                mach_vm_allocate(task, &mut addr, SIZE as u64, VM_FLAGS_ANYWHERE | flags)
            };
            assert_eq!(rc, 0, "mach_vm_allocate");
            Region {
                ptr: addr as *mut u8,
                free: Box::new(move |p| unsafe {
                    mach_vm_deallocate(task, p as u64, SIZE as u64);
                }),
            }
        }
        Backing::MemoryEntry => {
            let mut size = SIZE as u64;
            let mut handle = 0u32;
            let rc = unsafe {
                mach_make_memory_entry_64(
                    task,
                    &mut size,
                    0,
                    MAP_MEM_NAMED_CREATE | VM_PROT_RW,
                    &mut handle,
                    0,
                )
            };
            assert_eq!(rc, 0, "mach_make_memory_entry_64");
            let mut addr = 0u64;
            let rc = unsafe {
                mach_vm_map(
                    task,
                    &mut addr,
                    SIZE as u64,
                    0,
                    VM_FLAGS_ANYWHERE,
                    handle,
                    0,
                    0,
                    VM_PROT_RW,
                    VM_PROT_RW,
                    VM_INHERIT_NONE,
                )
            };
            assert_eq!(rc, 0, "mach_vm_map");
            Region {
                ptr: addr as *mut u8,
                free: Box::new(move |p| unsafe {
                    mach_vm_deallocate(task, p as u64, SIZE as u64);
                    mach_port_deallocate(task, handle);
                }),
            }
        }
        Backing::File => {
            let path = std::env::temp_dir().join(format!("lighter-memacc-{}", std::process::id()));
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            use std::os::unix::io::AsRawFd;
            assert_eq!(unsafe { libc::ftruncate(f.as_raw_fd(), SIZE as i64) }, 0);
            let p = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    f.as_raw_fd(),
                    0,
                )
            };
            assert!(
                p != libc::MAP_FAILED,
                "mmap file: {}",
                std::io::Error::last_os_error()
            );
            std::fs::remove_file(&path).unwrap();
            Region {
                ptr: p.cast(),
                free: Box::new(move |p| unsafe {
                    libc::munmap(p.cast(), SIZE);
                    drop(f);
                }),
            }
        }
        Backing::Shm => {
            let name =
                std::ffi::CString::new(format!("/lighter-memacc-{}", std::process::id())).unwrap();
            let fd = unsafe {
                libc::shm_open(
                    name.as_ptr(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                    0o600,
                )
            };
            assert!(fd >= 0, "shm_open: {}", std::io::Error::last_os_error());
            assert_eq!(unsafe { libc::ftruncate(fd, SIZE as i64) }, 0, "ftruncate");
            let p = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            assert!(
                p != libc::MAP_FAILED,
                "mmap shm: {}",
                std::io::Error::last_os_error()
            );
            unsafe {
                libc::shm_unlink(name.as_ptr());
                libc::close(fd);
            }
            Region {
                ptr: p.cast(),
                free: Box::new(|p| unsafe {
                    libc::munmap(p.cast(), SIZE);
                }),
            }
        }
    }
}

fn measure(label: &str) {
    let pid = std::process::id().to_string();
    let rss = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .unwrap();
    let rss_mb: u64 = String::from_utf8_lossy(&rss.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
        / 1024;
    let fp = Command::new("footprint").arg(&pid).output().unwrap();
    let fp = String::from_utf8_lossy(&fp.stdout);
    let phys = fp
        .lines()
        .find(|l| l.contains("phys_footprint:"))
        .map(|l| l.trim().to_string())
        .unwrap_or_default();
    let dirty = fp
        .lines()
        .find(|l| l.contains("Footprint:"))
        .map(|l| {
            l.split("Footprint:")
                .nth(1)
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .unwrap_or_default();
    let vm = Command::new("vmmap")
        .args(["--summary", &pid])
        .output()
        .unwrap();
    let vm = String::from_utf8_lossy(&vm.stdout);
    let region = vm
        .lines()
        .find(|l| l.contains(" 1.0G "))
        .map(|l| l.split_whitespace().take(4).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let vs = Command::new("vm_stat").output().unwrap();
    let vs = String::from_utf8_lossy(&vs.stdout);
    let pages = |key: &str| -> u64 {
        vs.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split(':').nth(1))
            .map(|v| v.trim().trim_end_matches('.').parse::<u64>().unwrap_or(0))
            .unwrap_or(0)
            * 16
            / 1024
    };
    println!(
        "{label:<40} rss={rss_mb}MB map-dirty={dirty} {phys} region=[{region}] sys-free={}MB sys-wired={}MB",
        pages("Pages free"),
        pages("Pages wired down")
    );
}

fn main() {
    let vm = Vm::create().expect("vm");
    let _gic = Gic::create(&vm, GicLayout::default()).expect("gic");
    let code = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    assert!(code != libc::MAP_FAILED);
    unsafe {
        std::ptr::copy_nonoverlapping(GUEST_CODE.as_ptr(), code.cast::<u32>(), GUEST_CODE.len())
    };
    unsafe { vm.map(code, CODE_BASE, PAGE, MemoryPerms::RWX) }.expect("map code");
    let mut vcpu = vm.create_vcpu().expect("vcpu");
    vcpu.set_trap_debug_exceptions(true).unwrap();

    measure("baseline");
    let cases: Vec<(&str, Backing, [&str; 3])> = vec![
        (
            "host first",
            Backing::Mmap(libc::MAP_PRIVATE | libc::MAP_NORESERVE),
            ["host touch", "then guest touch", "then host touch again"],
        ),
        (
            "file-backed, host first",
            Backing::File,
            ["host touch", "then guest touch", "then host touch again"],
        ),
        (
            "file-backed, guest first",
            Backing::File,
            ["guest touch", "then host touch", "then guest touch again"],
        ),
    ];
    for (label, backing, steps) in cases {
        let region = allocate(&backing);
        unsafe { vm.map(region.ptr.cast(), DATA_BASE, SIZE, MemoryPerms::RW) }.expect("map data");
        for step in steps {
            let guest = step.contains("guest");
            if guest {
                vcpu.set_reg(Reg::Pc, CODE_BASE).unwrap();
                vcpu.set_reg(Reg::Cpsr, lighter_hv::PSTATE_EL1H_DAIF_MASKED)
                    .unwrap();
                vcpu.set_reg(Reg::X0, DATA_BASE).unwrap();
                vcpu.set_reg(Reg::X1, DATA_BASE + SIZE as u64).unwrap();
                let started = std::time::Instant::now();
                match vcpu.run() {
                    Ok(Exit::Exception(e)) if e.class() == Exception::EC_BRK64 => {}
                    other => panic!("unexpected exit {other:?}"),
                }
                eprintln!("  guest touched 1 GiB in {:?}", started.elapsed());
            } else {
                let mut off = 0;
                while off < SIZE {
                    unsafe { region.ptr.add(off).write_volatile(1) };
                    off += 4096;
                }
            }
            measure(&format!("{label}: {step}"));
        }
        unsafe { vm.unmap(DATA_BASE, SIZE) }.expect("unmap");
        (region.free)(region.ptr);
        measure("  after unmap+free");
    }
}
