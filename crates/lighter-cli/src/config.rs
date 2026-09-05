//! What the user asked for.
//!
//! JSON rather than TOML for one reason: it is already a dependency, because
//! the Docker API speaks it. A second serialization format for six fields is
//! not worth the crate.

use serde::{Deserialize, Serialize};

/// How big a machine to build, and what to share with it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub cpus: u32,
    pub memory_mib: u64,
    /// Logical size of the disk Docker's images and volumes live on. Sparse, so
    /// this is a ceiling rather than a cost — and the ceiling matters to the
    /// filesystem inside it: btrfs lends a writer metadata against the space
    /// it has not allocated yet, and a small disk makes it flush a large copy
    /// mid-copy, file by file (see `Disk` in the architecture doc).
    pub disk_gib: u64,
    /// Directories from the Mac the guest can see, at the same paths.
    pub shares: Vec<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            // Half the cores, because the other half is what the Mac is for.
            // A guest given every core makes the machine it runs on unusable
            // while it builds, which is the loudest complaint about every tool
            // in this category.
            cpus: (num_cpus() / 2).max(2),
            // A quarter of physical memory. The guest gives back what it does
            // not use — see the balloon — so this is a ceiling and not a
            // reservation, and being generous costs nothing when idle.
            memory_mib: (physical_memory_mib() / 4).clamp(2048, 16384),
            // What the Mac has free when the machine is made, which is what
            // a sparse image could ever hold anyway, and is what OrbStack
            // gives its guest. Never less than 64 GiB: the image is sparse,
            // and a low ceiling is the only way to make btrfs slow.
            disk_gib: free_disk_gib().max(64),
            shares: vec![home_directory()],
        }
    }
}

impl Config {
    /// Reads the configuration, or the defaults if there is none.
    pub fn load() -> anyhow::Result<Config> {
        let path = crate::paths::config_file()?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = crate::paths::config_file()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

/// How long an idle vCPU may spin for a wakeup before it sleeps, in
/// nanoseconds (guest patch 0011's `idle.poll_ns`).
///
/// The spin is what makes a cross-vCPU wakeup a cache line instead of an
/// interrupt and a host scheduler round trip — most of an install's wall
/// time on the guest's own disk. It costs a host core while it lasts, and
/// on a Mac whose every core is a vCPU that core is the one the share's
/// server needed: on an eight-core M1 with eight vCPUs a pnpm install
/// through the share took 6.4–7.1 s at 200 µs and 5.3–6.4 at 50, with the
/// own-disk installs unchanged. Where the vCPUs leave cores free the longer
/// window is free too.
pub fn idle_poll_ns(cpus: u32) -> u32 {
    if cpus >= num_cpus() { 50_000 } else { 200_000 }
}

/// The guest kernel's tick rate: 250 unless `LIGHTER_KERNEL_HZ=1000`.
///
/// Every tick is an exit from the VM, per vCPU, and what the tick buys is
/// every wait the kernel counts in jiffies, which after expedited grace
/// periods is still most of a container's start and stop. Two kernels ship.
/// At 1000 a container's life on an M5 Pro is 60 ms against 112, and the
/// start-up's first container 0.47 s against 0.55; but the same A/B/A/B on
/// the share installs read pnpm 3.9–4.2 s at 250 against 4.2–4.9 and
/// 4.9–8.3 at 1000, npm level, yarn level to slightly worse, and on an M1
/// with four vCPUs npm 7% worse. The share installs are what most people do
/// most of the time, so 250 is the default on every machine, and 1000 is
/// there for a container-heavy day: compose stacks, test suites that start
/// containers by the hundred. The rule that picked by vCPUs against cores
/// was measured and dropped (`docs/worklog.md`, 2026-09-05).
pub fn kernel_hz(_cpus: u32) -> u32 {
    match std::env::var("LIGHTER_KERNEL_HZ").ok().as_deref() {
        Some("1000") => 1000,
        _ => 250,
    }
}

pub fn num_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
}

fn physical_memory_mib() -> u64 {
    let mut value: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: a static name, and an output buffer of exactly the stated size.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            &mut value as *mut u64 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && value > 0 {
        value / (1 << 20)
    } else {
        8192
    }
}

/// Free space on the volume the machine's image lives on, in GiB.
fn free_disk_gib() -> u64 {
    let home = home_directory();
    let Ok(path) = std::ffi::CString::new(home) else {
        return 0;
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and an out-parameter of the right type.
    if unsafe { libc::statfs(path.as_ptr(), &mut st) } != 0 {
        return 0;
    }
    (st.f_bavail as u64).saturating_mul(st.f_bsize as u64) >> 30
}

fn home_directory() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/Users".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine that takes every core makes the Mac it runs on unusable while
    /// it works, which is the complaint this whole project is answering.
    #[test]
    fn the_defaults_leave_the_mac_usable() {
        let config = Config::default();
        assert!(config.cpus >= 2);
        assert!(
            config.cpus <= num_cpus().max(2),
            "asked for more cores than exist"
        );
        assert!(config.memory_mib >= 2048);
        assert!(config.memory_mib <= physical_memory_mib() / 2);
    }

    #[test]
    fn a_config_round_trips() {
        let config = Config {
            cpus: 3,
            memory_mib: 4096,
            disk_gib: 32,
            shares: vec!["/tmp".into()],
        };
        let bytes = serde_json::to_vec(&config).unwrap();
        let back: Config = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.cpus, 3);
        assert_eq!(back.shares, vec!["/tmp".to_string()]);
    }

    /// A file written by an older version must still load, or an upgrade
    /// silently loses somebody's settings.
    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let back: Config = serde_json::from_str(r#"{"cpus": 3}"#).unwrap();
        assert_eq!(back.cpus, 3);
        assert_eq!(back.disk_gib, Config::default().disk_gib);
    }
}
