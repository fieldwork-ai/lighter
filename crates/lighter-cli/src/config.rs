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
    /// How the machine is sized: a fixed slice of the Mac (`fixed`), or the
    /// whole Mac with memory plugged in as it is needed (`native`).
    pub resources: Resources,
    /// Cores: the guest's in `fixed`, a cap in `native`. Unset is the
    /// mode's own choice (`Config::vcpus`).
    pub cpus: Option<u32>,
    /// Memory in MiB: the guest's ceiling in `fixed`, a cap in `native`.
    /// Unset is the mode's own choice (`Config::memory_mib`).
    pub memory_mib: Option<u64>,
    /// Logical size of the disk Docker's images and volumes live on. Sparse, so
    /// this is a ceiling rather than a cost — and the ceiling matters to the
    /// filesystem inside it: btrfs lends a writer metadata against the space
    /// it has not allocated yet, and a small disk makes it flush a large copy
    /// mid-copy, file by file (see `Disk` in the architecture doc).
    pub disk_gib: u64,
    /// Directories from the Mac the guest can see, at the same paths.
    pub shares: Vec<String>,
    /// Where a port a container publishes on every interface is bound on
    /// the Mac: the network (`lan`, as Docker does) or loopback only.
    pub publish: Publish,
    /// Whether the guest has a GPU: a render node containers reach Vulkan
    /// through (`docker run --device lighter.sh/gpu=all`), rendered on the
    /// Mac's own GPU. On by default; costs nothing until a container uses it.
    pub gpu: bool,
    /// Whether containers may run ONNX models on the Mac's Neural Engine
    /// (`docker run --device lighter.sh/ane=all`). On by default; ONNX
    /// Runtime is loaded on the host only when a model arrives.
    pub ane: bool,
    /// Whether containers may run PyTorch on the Mac's GPU
    /// (`docker run --device lighter.sh/mps=all`): the Mac's own `torch`
    /// executes what a container's `torch` asks, one operator at a time.
    /// On by default, and present only when a Python with torch and MPS is
    /// found (`torch_python`, or the first `python3` on PATH that has it).
    pub mps: bool,
    /// The Python whose `torch` serves `lighter.sh/mps`; empty means search.
    pub torch_python: String,
    /// Whether containers may run ggml (llama.cpp and friends, built with
    /// the RPC backend) on the Mac's GPU with ggml's own Metal kernels
    /// (`docker run --device lighter.sh/metal=all`). On by default.
    pub metal: bool,
    /// Whether containers get a V4L2 video decoder (`docker run --device
    /// lighter.sh/video=all`, `/dev/video0`): H.264 decoded by VideoToolbox
    /// on the Mac's media engine, for stock ffmpeg's `h264_v4l2m2m`. On by
    /// default; costs nothing until a container opens it.
    pub video: bool,
}

/// How the machine is sized.
///
/// `Fixed` is a slice of the Mac chosen up front: half the cores and a
/// quarter of the memory unless told otherwise, as every Mac container
/// runtime has always done. `Cooperative` shares the Mac as a native app
/// does: every core is a vCPU the Mac schedules like any other thread
/// (they run at the default QoS class, see `lighter_vmm::qos`), and the
/// guest boots on a small base with the rest plugged in through virtio-mem
/// as it is needed and given back when it is not, up to twice the Mac's
/// memory. Experimental.
///
/// `native` was this mode's name before 0.10.0 shipped; files and commands
/// that say it still mean it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Resources {
    #[default]
    Fixed,
    #[serde(alias = "native")]
    #[value(alias = "native")]
    Cooperative,
}

/// RAM a cooperative machine boots with, beside its virtio-mem range: enough
/// for the kernel, the engine and a small container without a plug.
pub const COOPERATIVE_BASE_BYTES: u64 = 2 << 30;

/// A cooperative machine's ceiling, as a multiple of the Mac's memory. Past
/// the Mac's own memory is what macOS compresses and swaps, as it does for a
/// native app that asks for that much: the policy grows the guest for page
/// cache only into memory the Mac is not using, and under pressure grows it
/// a step at a time, so what goes past is memory in use. Twice rather than
/// unbounded, so that a runaway container is stopped by the guest's OOM
/// killer before the Mac's swap fills its disk.
const COOPERATIVE_CEILING_FACTOR: u64 = 2;

/// A switch on the command line: `--gpu on`, `--gpu off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Toggle {
    On,
    Off,
}

impl From<Toggle> for bool {
    fn from(t: Toggle) -> bool {
        t == Toggle::On
    }
}

/// Who can reach a published port: `-p 8080:80` on every interface of the
/// Mac (`Lan`, Docker's meaning), or on loopback only (`Localhost`). A
/// publish with its own address (`-p 127.0.0.1:8080:80`) is bound as
/// asked under either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Publish {
    #[default]
    Lan,
    Localhost,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            resources: Resources::Fixed,
            cpus: None,
            memory_mib: None,
            // What the Mac has free when the machine is made, which is what
            // a sparse image could ever hold anyway, and is what OrbStack
            // gives its guest. Never less than 64 GiB: the image is sparse,
            // and a low ceiling is the only way to make btrfs slow.
            disk_gib: free_disk_gib().max(64),
            shares: vec![home_directory()],
            publish: Publish::Lan,
            gpu: true,
            ane: true,
            mps: true,
            torch_python: String::new(),
            metal: true,
            video: true,
        }
    }
}

impl Config {
    /// The guest's cores.
    pub fn vcpus(&self) -> u32 {
        let host = num_cpus();
        match self.resources {
            // Half the cores, because the other half is what the Mac is for.
            // A guest given every core makes the machine it runs on unusable
            // while it builds, which is the loudest complaint about every
            // tool in this category.
            Resources::Fixed => self.cpus.unwrap_or((host / 2).max(2)),
            // Every core, at the default QoS class: a build competes with the
            // Mac's windows as a native build does, and loses to them.
            Resources::Cooperative => self.cpus.map_or(host, |cap| cap.clamp(1, host)),
        }
    }

    /// The guest's memory ceiling, in MiB.
    pub fn memory_mib(&self) -> u64 {
        let host = physical_memory_mib();
        match self.resources {
            // A quarter of physical memory. The guest gives back what it does
            // not use — see the balloon — so resident pages grow on demand.
            // Backing-object metadata and initialization still scale with
            // this ceiling, even while the guest is idle.
            Resources::Fixed => self.memory_mib.unwrap_or((host / 4).clamp(2048, 16384)),
            // Twice the Mac's (`COOPERATIVE_CEILING_FACTOR`). Only the plugged
            // part of the range costs anything, page array included.
            Resources::Cooperative => {
                let ceiling = host.saturating_mul(COOPERATIVE_CEILING_FACTOR).max(2048);
                // And what the widest address space this Mac's hypervisor
                // allows can hold, beside the GPU's and the decoder's windows.
                let ceiling = lighter_vmm::machine::max_ram_bytes(
                    crate::run::GPU_APERTURE_BYTES,
                    crate::run::VIDEO_APERTURE_BYTES,
                )
                .map_or(ceiling, |max| ceiling.min(max >> 20));
                self.memory_mib
                    .map_or(ceiling, |cap| cap.clamp(2048, ceiling))
            }
        }
    }

    /// RAM to boot with and the virtio-mem range beside it, in bytes.
    pub fn memory_split(&self) -> (u64, u64) {
        let total = self.memory_mib() << 20;
        match self.resources {
            Resources::Fixed => (total, 0),
            Resources::Cooperative => {
                lighter_vmm::virtio::mem::split(total, COOPERATIVE_BASE_BYTES)
            }
        }
    }

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

/// The rest of the idle poll's policy for the same machine, as kernel
/// command-line arguments (guest patch 0039).
///
/// Where the vCPUs leave cores free, a poll that times out halves the
/// window and polling backs off above 50 µs of spinning a wakeup caught:
/// Frigate on one camera, 16 vCPUs of 18, went from about 20% of a core to
/// 15. Where they fill the cores the window is already short, and halving
/// it on every timeout cost an eight-core M1's npm install on the guest's
/// disk 12% (8.75 s against 7.8); there the window keeps 0011's rules and
/// the backoff waits for 100 µs a catch (8.2 s, pnpm unchanged).
pub fn idle_poll_args(cpus: u32) -> &'static str {
    if cpus >= num_cpus() {
        " idle.poll_timeouts=0 idle.poll_cost_ns=100000"
    } else {
        ""
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
        assert_eq!(config.resources, Resources::Fixed);
        assert!(config.vcpus() >= 2);
        assert!(
            config.vcpus() <= num_cpus().max(2),
            "asked for more cores than exist"
        );
        assert!(config.memory_mib() >= 2048);
        assert!(config.memory_mib() <= physical_memory_mib() / 2);
        assert_eq!(config.memory_split().1, 0, "fixed has no virtio-mem range");
    }

    /// The ceiling a cooperative machine gets on this Mac: twice its memory,
    /// within what the hypervisor's address space can hold.
    fn cooperative_ceiling() -> u64 {
        let ceiling = physical_memory_mib()
            .saturating_mul(COOPERATIVE_CEILING_FACTOR)
            .max(2048);
        lighter_vmm::machine::max_ram_bytes(
            crate::run::GPU_APERTURE_BYTES,
            crate::run::VIDEO_APERTURE_BYTES,
        )
        .map_or(ceiling, |max| ceiling.min(max >> 20))
    }

    /// Cooperative is every core, and a ceiling past the Mac's memory, booted
    /// on a small base with the rest a range.
    #[test]
    fn cooperative_shares_the_whole_mac() {
        let config = Config {
            resources: Resources::Cooperative,
            ..Config::default()
        };
        assert_eq!(config.vcpus(), num_cpus());
        assert_eq!(config.memory_mib(), cooperative_ceiling());
        let (base, range) = config.memory_split();
        assert_eq!(base + range, config.memory_mib() << 20);
        assert!(base <= COOPERATIVE_BASE_BYTES.max(config.memory_mib() << 20));
    }

    /// In cooperative, a count is a cap: never above the ceiling, never below
    /// one core or two gigabytes.
    #[test]
    fn cooperative_counts_are_caps() {
        let config = Config {
            resources: Resources::Cooperative,
            cpus: Some(2),
            memory_mib: Some(4096),
            ..Config::default()
        };
        assert_eq!(config.vcpus(), 2.min(num_cpus()));
        assert_eq!(config.memory_mib(), 4096.clamp(2048, cooperative_ceiling()));
        let config = Config {
            resources: Resources::Cooperative,
            cpus: Some(10_000),
            memory_mib: Some(1 << 30),
            ..Config::default()
        };
        assert_eq!(config.vcpus(), num_cpus());
        assert_eq!(config.memory_mib(), cooperative_ceiling());
    }

    /// `native`, the mode's name until 0.10.0 shipped, still reads as it, in a
    /// file and on the command line; what is written back is the new name.
    #[test]
    fn native_is_the_old_name_for_cooperative() {
        let back: Config = serde_json::from_str(r#"{"resources":"native"}"#).unwrap();
        assert_eq!(back.resources, Resources::Cooperative);
        assert!(
            serde_json::to_string(&back)
                .unwrap()
                .contains(r#""resources":"cooperative""#)
        );
        use clap::ValueEnum;
        assert_eq!(
            Resources::from_str("native", false).unwrap(),
            Resources::Cooperative
        );
        assert_eq!(
            Resources::from_str("cooperative", false).unwrap(),
            Resources::Cooperative
        );
    }

    /// An existing file's numbers keep meaning what they meant.
    #[test]
    fn fixed_counts_are_the_allocation() {
        let config = Config {
            cpus: Some(3),
            memory_mib: Some(4096),
            ..Config::default()
        };
        assert_eq!(config.vcpus(), 3);
        assert_eq!(config.memory_mib(), 4096);
        assert_eq!(config.memory_split(), (4096 << 20, 0));
    }

    #[test]
    fn a_config_round_trips() {
        let config = Config {
            resources: Resources::Cooperative,
            cpus: Some(3),
            memory_mib: Some(4096),
            disk_gib: 32,
            shares: vec!["/tmp".into()],
            publish: Publish::Localhost,
            gpu: false,
            ane: false,
            mps: false,
            torch_python: String::new(),
            metal: false,
            video: false,
        };
        let bytes = serde_json::to_vec(&config).unwrap();
        assert!(
            std::str::from_utf8(&bytes)
                .unwrap()
                .contains(r#""publish":"localhost""#),
            "the scope is written as a word a person can read and type"
        );
        assert!(
            std::str::from_utf8(&bytes)
                .unwrap()
                .contains(r#""resources":"cooperative""#)
        );
        let back: Config = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.resources, Resources::Cooperative);
        assert_eq!(back.cpus, Some(3));
        assert_eq!(back.shares, vec!["/tmp".to_string()]);
        assert_eq!(back.publish, Publish::Localhost);
    }

    /// A file written by an older version must still load, or an upgrade
    /// silently loses somebody's settings.
    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let back: Config = serde_json::from_str(r#"{"cpus": 3, "memory_mib": 4096}"#).unwrap();
        assert_eq!(back.resources, Resources::Fixed);
        assert_eq!(back.cpus, Some(3));
        assert_eq!(back.memory_mib(), 4096);
        assert_eq!(back.disk_gib, Config::default().disk_gib);
        assert_eq!(back.publish, Publish::Lan);
    }
}
