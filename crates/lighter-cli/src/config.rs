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
    /// Directories from the Mac the guest can see, at the same paths
    /// ([`DEFAULT_SHARES`] unless changed with `lighter config --share`).
    pub shares: Vec<String>,
    /// What the file was written to mean. Missing, it is from before 0.11.6,
    /// whose only share was the home folder: that reads as the defaults,
    /// once, and a home folder chosen since is taken as chosen.
    #[serde(default)]
    pub format: u32,
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
    /// USB devices on the Mac the guest has, as if plugged into it (`lighter
    /// usb attach`), attached whenever they are plugged in and the machine
    /// runs.
    pub usb: Vec<UsbDevice>,
    /// Whether the machine joins the Mac's network with a card of its own
    /// ("LAN mode", 0.12), so a host-network container can discover and
    /// be discovered. Off by default.
    pub lan: bool,
    /// Which of the Mac's cards it bridges: `auto` for the primary.
    pub lan_interface: String,
    /// Its address on the LAN: empty for DHCP, or `192.168.50.240[/24]`
    /// where the router cannot lease one (Wi-Fi, mostly).
    pub lan_address: String,
    /// Whether the Mac reaches containers directly, at their own addresses
    /// and as `name.lighter.local`, over a network of the Mac and the
    /// machine alone (`link.rs`, 0.13). Off by default: a vmnet host network
    /// has macOS turn its packet filter on for the whole Mac while it is up,
    /// which costs every app's loopback a third of its throughput and
    /// lighter's published ports 15-20% (measured 2026-10-09).
    pub direct: bool,
    /// The /16 that network and Docker's networks are on: empty for the
    /// one chosen when it was first made.
    pub direct_subnet: String,
}

/// A USB device the guest has: `vendor:product[:serial]`, in hex, and
/// whether it was attached against a default refusal (`--force`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbDevice {
    pub spec: String,
    #[serde(default)]
    pub force: bool,
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
            shares: default_shares(),
            format: FORMAT,
            publish: Publish::Lan,
            gpu: true,
            ane: true,
            mps: true,
            torch_python: String::new(),
            metal: true,
            video: true,
            usb: Vec::new(),
            lan: false,
            lan_interface: "auto".into(),
            lan_address: String::new(),
            direct: false,
            direct_subnet: String::new(),
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
            Ok(bytes) => Config::read(&bytes, &home_directory()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn read(bytes: &[u8], home: &str) -> anyhow::Result<Config> {
        let mut config: Config = serde_json::from_slice(bytes)?;
        if config.format < FORMAT {
            config.shares = upgraded_shares(std::mem::take(&mut config.shares), home);
            config.format = FORMAT;
        }
        Ok(config)
    }

    /// Shares a folder from the Mac. One inside a share already is shared,
    /// and says by which.
    pub fn share(&mut self, path: &str) -> Result<(), String> {
        let path = path.trim_end_matches('/');
        if !path.starts_with('/') {
            return Err(format!("{path}: give the folder's full path, from /"));
        }
        if let Some(by) = self.shares.iter().find(|s| within(path, s)) {
            return Err(format!("{path} is already shared, as part of {by}"));
        }
        self.shares.push(path.to_string());
        self.shares = normalized_shares(std::mem::take(&mut self.shares));
        Ok(())
    }

    /// Stops sharing a folder. Only a share as a whole can go: a folder
    /// inside one is that share's.
    pub fn unshare(&mut self, path: &str) -> Result<(), String> {
        let path = path.trim_end_matches('/');
        if let Some(at) = self.shares.iter().position(|s| s == path) {
            self.shares.remove(at);
            return Ok(());
        }
        match self.shares.iter().find(|s| within(path, s)) {
            Some(by) => Err(format!(
                "{path} is part of {by}, which is shared as a whole; `--unshare {by}` stops sharing all of it"
            )),
            None => Err(format!("{path} is not shared")),
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

/// The current meaning of a config file; see [`Config::format`].
const FORMAT: u32 = 1;

fn home_directory() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/Users".into())
}

/// What containers can bind-mount from the Mac unless told otherwise:
/// Docker Desktop's file-sharing defaults (`/Users`, `/Volumes`, `/private`,
/// `/tmp`, `/var/folders`), less the two lighter cannot mount where Docker
/// Desktop does. A share is mounted in the guest at its own path, so the
/// Mac's `/tmp` would hide the machine's own, which dockerd uses; and
/// `/private` would serve `$TMPDIR` a second time beside `/var/folders`,
/// from a second server whose cache the first's writes never reach.
pub const DEFAULT_SHARES: [&str; 3] = ["/Users", "/Volumes", "/var/folders"];

pub fn default_shares() -> Vec<String> {
    DEFAULT_SHARES.iter().map(|s| s.to_string()).collect()
}

/// Until 0.11.6 the home folder was the one share, written into the file at
/// install; a list naming it is read as the defaults, with whatever else it
/// named beside them (a drive under `/Volumes` is then already covered).
pub fn upgraded_shares(shares: Vec<String>, home: &str) -> Vec<String> {
    let home = home.trim_end_matches('/');
    if !shares.iter().any(|s| s.trim_end_matches('/') == home) {
        return normalized_shares(shares);
    }
    normalized_shares(
        default_shares()
            .into_iter()
            .chain(
                shares
                    .into_iter()
                    .filter(|s| s.trim_end_matches('/') != home),
            )
            .collect(),
    )
}

/// The shares, without duplicates or a path inside another: a directory is
/// served by one share, or the guest mounts one over the other.
pub fn normalized_shares(shares: Vec<String>) -> Vec<String> {
    let mut paths: Vec<String> = shares
        .into_iter()
        .map(|s| {
            let trimmed = s.trim_end_matches('/');
            if trimmed.is_empty() {
                "/".into()
            } else {
                trimmed.to_string()
            }
        })
        .collect();
    paths.sort_by_key(|p| p.len());
    let mut kept: Vec<String> = Vec::new();
    for path in paths {
        if !kept.iter().any(|k| within(&path, k)) {
            kept.push(path);
        }
    }
    let order = |p: &String| {
        DEFAULT_SHARES
            .iter()
            .position(|d| d == p)
            .unwrap_or(usize::MAX)
    };
    kept.sort_by(|a, b| order(a).cmp(&order(b)).then(a.cmp(b)));
    kept
}

/// Whether `path` is `base` or inside it, by whole components.
pub fn within(path: &str, base: &str) -> bool {
    let base = base.trim_end_matches('/');
    base.is_empty() || path == base || path.starts_with(&format!("{base}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The home folder alone, as every install before 0.11.6 wrote it, reads
    /// as the defaults; a drive listed beside it is inside `/Volumes`.
    #[test]
    fn a_config_from_before_the_defaults_reads_as_them() {
        assert_eq!(
            upgraded_shares(vec!["/Users/nick".into()], "/Users/nick"),
            default_shares()
        );
        assert_eq!(
            upgraded_shares(
                vec![
                    "/Users/nick/".into(),
                    "/Volumes/Media".into(),
                    "/opt/data".into()
                ],
                "/Users/nick"
            ),
            ["/Users", "/Volumes", "/var/folders", "/opt/data"]
        );
    }

    /// The upgrade happens to a file from before the defaults, once: the
    /// home folder alone, chosen since, stays the home folder alone.
    #[test]
    fn a_home_folder_chosen_since_is_kept() {
        let old = Config::read(br#"{"shares": ["/Users/nick"]}"#, "/Users/nick").unwrap();
        assert_eq!(old.shares, default_shares());
        let chosen = Config {
            shares: vec!["/Users/nick".into()],
            ..Config::default()
        };
        let bytes = serde_json::to_vec(&chosen).unwrap();
        assert_eq!(
            Config::read(&bytes, "/Users/nick").unwrap().shares,
            ["/Users/nick"]
        );
    }

    /// A list that no longer names the home folder is the user's own.
    #[test]
    fn a_list_without_the_home_folder_is_kept() {
        assert_eq!(
            upgraded_shares(vec!["/Users".into(), "/var/folders".into()], "/Users/nick"),
            ["/Users", "/var/folders"]
        );
    }

    #[test]
    fn sharing_and_unsharing() {
        let mut config = Config {
            shares: default_shares(),
            ..Config::default()
        };
        assert!(
            config
                .share("/Volumes/T9")
                .unwrap_err()
                .contains("part of /Volumes")
        );
        config.share("/opt/data/").unwrap();
        assert!(config.shares.contains(&"/opt/data".to_string()));
        assert!(config.share("relative").is_err());
        assert!(
            config
                .unshare("/Volumes/T9")
                .unwrap_err()
                .contains("--unshare /Volumes")
        );
        config.unshare("/Volumes").unwrap();
        assert_eq!(config.shares, ["/Users", "/var/folders", "/opt/data"]);
        assert!(config.unshare("/nowhere").is_err());
    }

    #[test]
    fn a_share_inside_another_is_dropped() {
        assert_eq!(
            normalized_shares(vec![
                "/Volumes/T9".into(),
                "/Volumes".into(),
                "/Volumes".into(),
                "/Users2".into()
            ]),
            ["/Volumes", "/Users2"]
        );
        assert!(within("/Volumes/T9/x", "/Volumes"));
        assert!(!within("/Volumes2", "/Volumes"));
    }

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
            format: FORMAT,
            publish: Publish::Localhost,
            gpu: false,
            ane: false,
            mps: false,
            torch_python: String::new(),
            metal: false,
            video: false,
            usb: vec![UsbDevice {
                spec: "303a:831a".into(),
                force: false,
            }],
            lan: true,
            lan_interface: "en1".into(),
            lan_address: "192.168.50.240/24".into(),
            direct: false,
            direct_subnet: "10.240.0.0/16".into(),
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
        assert!(back.lan);
        assert!(!back.direct);
        assert_eq!(back.direct_subnet, "10.240.0.0/16");
        assert_eq!(
            (back.lan_interface.as_str(), back.lan_address.as_str()),
            ("en1", "192.168.50.240/24")
        );
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
