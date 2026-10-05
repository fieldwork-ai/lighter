//! `lighter` — Docker on a Mac, on a virtual machine we wrote.
//!
//! The command a person types. Everything it does is either arranging for a
//! machine process to exist or asking one a question; the machine itself is
//! [`lighter_vmm`], and the `run` subcommand is the one that becomes it.
//!
//! Two processes rather than one, because a VM has to outlive the terminal
//! that started it. That is also what lets launchd own it, and what makes
//! `lighter status` answerable by anyone rather than only by whoever is
//! holding the console.

mod ane_host;
mod bundle;
mod config;
mod context;
mod doctor;
mod installation;
mod instance;
mod localnet;
mod machine;
mod mps;
mod paths;
mod release;
mod run;
mod service;
mod storage_status;
mod updates;
mod upgrade;
mod usb;

use std::time::Duration;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "lighter",
    version,
    about = "Docker for macOS, on a virtual machine built for it",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum UsbAction {
    /// The Mac's USB devices, and which the guest has.
    #[command(alias = "ls")]
    List,
    /// Attach a device, by vendor:product (hex, from `lighter usb list`),
    /// with :serial when two of the same are plugged in. It stays attached
    /// across restarts and re-plugs until detached.
    Attach {
        spec: String,
        /// Attach an input or storage device, or one whose port a Mac
        /// program has open, which are refused otherwise.
        #[arg(long)]
        force: bool,
    },
    /// Give a device back to macOS.
    Detach { spec: String },
    /// The guest's /dev/serial/by-id names, for `docker run --device`.
    #[command(name = "ls-serial")]
    LsSerial,
}

#[derive(Subcommand)]
enum Command {
    /// Start the machine and point the Docker CLI at it.
    Start {
        /// How long to wait for Docker to answer.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Stop the machine.
    Stop,
    /// Gives attached USB devices back to macOS when the machine's process
    /// exits; started by the machine.
    #[command(hide = true, name = "usb-keeper")]
    UsbKeeper {
        #[arg(long)]
        parent: u32,
    },
    /// USB devices on the Mac, attached to the guest as if plugged into it.
    Usb {
        #[command(subcommand)]
        action: UsbAction,
    },
    /// Restart the machine.
    Restart,
    /// Say whether it is running, and what it costs.
    Status,
    /// Check that this Mac can run lighter, and say what to fix if not.
    Doctor,
    /// The Neural Engine service, run as a child of `lighter start`.
    #[command(hide = true, name = "ane-host")]
    AneHost {
        #[arg(long)]
        port: u16,
        #[arg(long)]
        cache: std::path::PathBuf,
    },
    /// Rosetta for amd64 containers: whether this Mac has it, and installing it.
    Rosetta {
        /// Run Apple's installer for Rosetta.
        #[arg(long)]
        install: bool,
    },
    /// Show the machine's log.
    Logs {
        /// Follow the log rather than printing what is there.
        #[arg(short, long)]
        follow: bool,
    },
    /// Show or change the configuration.
    Config {
        /// How the machine is sized: a fixed slice of the Mac (`fixed`, the
        /// default), or cooperative resources (`cooperative`, experimental):
        /// every core, and memory plugged in as it is needed, up to twice the
        /// Mac's, and given back when it is not.
        #[arg(long, value_enum)]
        resources: Option<config::Resources>,
        /// Cores to give the guest; in `cooperative`, the most it may use.
        #[arg(long)]
        cpus: Option<u32>,
        /// Memory ceiling, in MiB; in `cooperative`, the most it may use. The
        /// guest gives back what it does not use.
        #[arg(long)]
        memory: Option<u64>,
        /// Size of the disk images and volumes live on, in GiB.
        #[arg(long)]
        disk: Option<u64>,
        /// Who can reach a published port: the network (`lan`, as Docker
        /// does) or loopback (`localhost`); explicit bind addresses take precedence.
        #[arg(long, value_enum)]
        publish: Option<config::Publish>,
        /// Whether the guest has a GPU (`on`, the default, or `off`).
        #[arg(long, value_enum)]
        gpu: Option<config::Toggle>,
        /// Whether containers may use the Neural Engine (`on`, the default, or `off`).
        #[arg(long, value_enum)]
        ane: Option<config::Toggle>,
        /// Whether containers may run PyTorch on the Mac's GPU (`on`, the default, or `off`).
        #[arg(long, value_enum)]
        mps: Option<config::Toggle>,
        /// The Python whose torch serves the PyTorch device; `auto` to search PATH.
        #[arg(long)]
        torch_python: Option<String>,
        /// Whether containers may run ggml on the Mac's GPU (`on`, the default, or `off`).
        #[arg(long, value_enum)]
        metal: Option<config::Toggle>,
        /// Whether containers get a hardware video decoder (`on`, the default, or `off`).
        #[arg(long, value_enum)]
        video: Option<config::Toggle>,
        /// Share a folder from the Mac with containers, at the same path
        /// (`/Users`, `/Volumes` and `/var/folders` are shared already).
        #[arg(long, value_name = "PATH")]
        share: Vec<String>,
        /// Stop sharing a folder.
        #[arg(long, value_name = "PATH")]
        unshare: Vec<String>,
    },
    /// Put the guest's clock right.
    ///
    /// Done automatically when the Mac wakes; this is the same thing, for when
    /// you want to check it or something has drifted anyway.
    Resync,
    /// Start lighter when you log in.
    Install,
    /// Stop starting lighter when you log in.
    Uninstall,
    /// Check for and download stable releases without changing the running VM.
    Update {
        #[command(subcommand)]
        action: updates::Action,
    },
    /// Activate a verified release (direct installations only).
    Upgrade {
        /// Explicitly allow stopping and restarting a running VM.
        #[arg(long)]
        restart: bool,
    },
    #[command(hide = true)]
    AdoptRelease {
        #[arg(long)]
        source: std::path::PathBuf,
        #[arg(long)]
        prefix: std::path::PathBuf,
        #[arg(long)]
        restart: bool,
    },
    #[command(hide = true)]
    ServiceRefresh,
    #[command(hide = true)]
    InstallArchive {
        #[arg(long)]
        archive: std::path::PathBuf,
        #[arg(long)]
        prefix: std::path::PathBuf,
        #[arg(long)]
        restart: bool,
    },
    /// Become the machine. Not for typing; `start` runs this.
    #[command(hide = true)]
    Run,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    if !matches!(
        &cli.command,
        Command::Run
            | Command::Update { .. }
            | Command::AdoptRelease { .. }
            | Command::ServiceRefresh
            | Command::InstallArchive { .. }
    ) {
        updates::notice();
        if let Err(e) = service::heal() {
            eprintln!("lighter: could not point the login agent at the installed release: {e}");
        }
    }
    match dispatch(cli.command) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("lighter: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn dispatch(command: Command) -> anyhow::Result<std::process::ExitCode> {
    match command {
        Command::InstallArchive {
            archive,
            prefix,
            restart,
        } => {
            let temp = tempfile::tempdir()?;
            let source = release::extract(&archive, temp.path())?;
            upgrade::install(&source, &prefix, restart)?;
            Ok(std::process::ExitCode::SUCCESS)
        }
        Command::Update { action } => {
            updates::run(action)?;
            Ok(std::process::ExitCode::SUCCESS)
        }
        Command::Upgrade { restart } => {
            upgrade::run(restart)?;
            Ok(std::process::ExitCode::SUCCESS)
        }
        Command::AdoptRelease {
            source,
            prefix,
            restart,
        } => {
            upgrade::install(&source, &prefix, restart)?;
            Ok(std::process::ExitCode::SUCCESS)
        }
        Command::ServiceRefresh => {
            let root = installation::payload(&std::env::current_exe()?)
                .ok_or_else(|| anyhow::anyhow!("not a packaged release"))?;
            service::refresh_at(&root, false)?;
            Ok(std::process::ExitCode::SUCCESS)
        }
        Command::Run => {
            run::machine()?;
            Ok(std::process::ExitCode::SUCCESS)
        }
        Command::Start { timeout } => start(Duration::from_secs(timeout)),
        Command::Stop => stop(),
        // The context stays on lighter across a restart: going back to the
        // previous one and returning would lose it if its daemon were down.
        Command::Restart => {
            stop_machine()?;
            start(Duration::from_secs(120))
        }
        Command::Status => status(),
        Command::Usb { action } => match action {
            UsbAction::List => usb::list(),
            UsbAction::Attach { spec, force } => usb::attach(&spec, force),
            UsbAction::Detach { spec } => usb::detach(&spec),
            UsbAction::LsSerial => usb::ls_serial(),
        },
        Command::UsbKeeper { parent } => usb::keeper(parent),
        Command::AneHost { port, cache } => {
            ane_host::serve(port, &cache)?;
            Ok(std::process::ExitCode::SUCCESS)
        }
        Command::Doctor => {
            let findings = doctor::run();
            print!("{}", doctor::report(&findings));
            Ok(if findings.iter().all(|f| f.ok) {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::FAILURE
            })
        }
        Command::Rosetta { install } => {
            use lighter_vmm::rosetta;
            if install {
                if rosetta::installed() {
                    println!("Rosetta is already installed.");
                } else {
                    // Apple's own installer; the flag is its, and stands in for
                    // the agreement its interactive form shows.
                    let status = std::process::Command::new("/usr/sbin/softwareupdate")
                        .args(["--install-rosetta", "--agree-to-license"])
                        .status()
                        .map_err(|e| anyhow::anyhow!("running softwareupdate: {e}"))?;
                    if !status.success() || !rosetta::installed() {
                        anyhow::bail!("Rosetta did not install ({status})");
                    }
                    println!(
                        "Rosetta installed. Restart lighter to run amd64 containers under it."
                    );
                }
            } else if rosetta::installed() {
                match rosetta::key() {
                    Ok(_) => println!("installed: amd64 containers run under Rosetta"),
                    Err(e) => println!("installed but not usable by lighter: {e}"),
                }
            } else {
                println!("not installed: amd64 containers run under emulation");
                println!("run `lighter rosetta --install` to install it");
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        Command::Logs { follow } => logs(follow),
        Command::Config {
            resources,
            cpus,
            memory,
            disk,
            publish,
            gpu,
            ane,
            mps,
            torch_python,
            metal,
            video,
            share,
            unshare,
        } => configure(Settings {
            resources,
            cpus,
            memory,
            disk,
            publish,
            gpu,
            ane,
            mps,
            torch_python,
            metal,
            video,
            share,
            unshare,
        }),
        Command::Resync => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            match machine::control(&format!("time {now}"))? {
                reply if reply == "ok" => {
                    println!("Guest clock set.");
                    Ok(std::process::ExitCode::SUCCESS)
                }
                reply => anyhow::bail!("the guest refused: {reply}"),
            }
        }
        Command::Install => {
            service::install()?;
            println!("lighter will start when you log in.");
            Ok(std::process::ExitCode::SUCCESS)
        }
        Command::Uninstall => {
            service::uninstall()?;
            println!("lighter will no longer start when you log in.");
            Ok(std::process::ExitCode::SUCCESS)
        }
    }
}

fn start(timeout: Duration) -> anyhow::Result<std::process::ExitCode> {
    let _total = lighter_vmm::boot_timing::Phase::new("cli_start");
    let config = config::Config::load()?;
    // Checked before starting rather than after failing: a missing kernel
    // produces a machine that exits immediately, and the log says less than
    // this does.
    let doctor_phase = lighter_vmm::boot_timing::Phase::new("cli_doctor");
    let findings = doctor::run();
    drop(doctor_phase);
    let docker_available = findings.iter().any(|f| f.what == "docker client" && f.ok);
    let blocking: Vec<_> = findings
        .into_iter()
        .filter(|f| {
            !f.ok
                && !matches!(
                    f.what.as_str(),
                    "docker client"
                        | "docker context"
                        | "machine"
                        | "rosetta"
                        | "lighter.sh/gpu"
                        | "lighter.sh/ane"
                        | "lighter.sh/metal"
                        | "lighter.sh/video"
                        | "local network"
                )
        })
        .collect();
    if !blocking.is_empty() {
        eprint!("{}", doctor::report(&blocking));
        anyhow::bail!("cannot start; see `lighter doctor`");
    }

    println!(
        "Starting lighter ({} cores, {} MiB{})…",
        config.vcpus(),
        config.memory_mib(),
        match config.resources {
            config::Resources::Fixed => "",
            config::Resources::Cooperative => ", cooperative resources",
        }
    );
    let pid = machine::start(&config, timeout)?;
    let socket = paths::docker_socket()?;
    let version = machine::docker_version(&socket)?;
    println!("Docker {version}");
    if paths::is_default_home() && docker_available {
        context::install(&socket, &paths::previous_context()?)?;
        println!("Running as pid {pid}; the docker CLI now points at it.");
    } else {
        println!(
            "Running as pid {pid}; use DOCKER_HOST=unix://{}",
            socket.display()
        );
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn stop() -> anyhow::Result<std::process::ExitCode> {
    // The context goes first. A CLI pointed at a socket that is about to
    // vanish fails in a way that reads as Docker being broken. A custom home
    // never owned the context, so it has nothing to put back.
    if paths::is_default_home() {
        let _ = context::release(&paths::previous_context()?);
    }
    stop_machine()
}

fn stop_machine() -> anyhow::Result<std::process::ExitCode> {
    if machine::stop(Duration::from_secs(30))? {
        println!("Stopped.");
    } else {
        println!("Not running.");
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn status() -> anyhow::Result<std::process::ExitCode> {
    let status = machine::status()?;
    print!("{}", installation::version_report());
    if !status.running {
        println!("lighter is not running.");
        return Ok(std::process::ExitCode::from(1));
    }
    println!("lighter is running.");
    if let Some(pid) = status.pid {
        println!("  pid        {pid}");
    }
    match &status.docker {
        Some(version) => println!("  docker     {version}"),
        None => println!("  docker     not answering yet"),
    }
    if let Some(mib) = status.footprint_mib {
        println!("  memory     {mib} MiB");
    }
    if let Some(memory) = status.memory {
        println!(
            "  guest      {} of {} MiB plugged in (cooperative resources)",
            memory.base_mib + memory.plugged_mib,
            memory.base_mib + memory.range_mib
        );
    }
    if let Some(entries) = usb::status() {
        for e in entries {
            let detail = if e.detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", e.detail)
            };
            println!("  usb        {} {}{detail}", e.spec, e.status);
        }
    }
    if let Some(restarts) = &status.agent_restarts {
        println!("  agents     restarted: {restarts} (see `lighter logs`)");
    }
    for port in &status.unforwarded {
        println!(
            "  ports      {} {} not forwarded on {}, retrying: {}",
            port.proto,
            port.port,
            port.addrs.join(", "),
            port.reason
        );
    }
    for disk in &status.storage_waiting {
        println!("  storage    Waiting for host disk space; VM running, writes waiting.");
        println!(
            "             {}: {}, {}s, {} retries",
            disk.disk.display(),
            disk.operation,
            disk.waiting_seconds,
            disk.retries
        );
    }
    println!("  socket     {}", paths::docker_socket()?.display());
    Ok(std::process::ExitCode::SUCCESS)
}

fn logs(follow: bool) -> anyhow::Result<std::process::ExitCode> {
    let path = paths::log_file()?;
    if !path.exists() {
        anyhow::bail!("no log at {}; has it ever started?", path.display());
    }
    let mut command = std::process::Command::new("/usr/bin/tail");
    if follow {
        command.arg("-f");
    } else {
        command.args(["-n", "200"]);
    }
    let status = command.arg(&path).status()?;
    Ok(if status.success() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}

/// What `lighter config` was asked to change; `None` leaves a setting alone.
#[derive(Default)]
struct Settings {
    resources: Option<config::Resources>,
    cpus: Option<u32>,
    memory: Option<u64>,
    disk: Option<u64>,
    publish: Option<config::Publish>,
    gpu: Option<config::Toggle>,
    ane: Option<config::Toggle>,
    mps: Option<config::Toggle>,
    torch_python: Option<String>,
    metal: Option<config::Toggle>,
    video: Option<config::Toggle>,
    share: Vec<String>,
    unshare: Vec<String>,
}

/// Writes what `lighter config` was given into `config`, saying whether
/// anything was given. A share that cannot be made changes nothing.
fn apply(config: &mut config::Config, settings: Settings) -> Result<bool, String> {
    let Settings {
        resources,
        cpus,
        memory,
        disk,
        publish,
        gpu,
        ane,
        mps,
        torch_python,
        metal,
        video,
        share,
        unshare,
    } = settings;
    let changed = resources.is_some()
        || cpus.is_some()
        || memory.is_some()
        || disk.is_some()
        || publish.is_some()
        || gpu.is_some()
        || ane.is_some()
        || mps.is_some()
        || torch_python.is_some()
        || metal.is_some()
        || video.is_some()
        || !share.is_empty()
        || !unshare.is_empty();
    for path in &unshare {
        config.unshare(path)?;
    }
    for path in &share {
        if !std::path::Path::new(path).is_dir() {
            return Err(format!("{path} is not a folder on this Mac"));
        }
        config.share(path)?;
    }
    if let Some(resources) = resources
        && resources != config.resources
    {
        // A count chosen for the other mode means something else in this one
        // (an allocation in fixed, a cap in native), so it does not carry
        // over; one given alongside the switch is kept below.
        config.resources = resources;
        config.cpus = None;
        config.memory_mib = None;
    }
    if let Some(cpus) = cpus {
        config.cpus = Some(cpus);
    }
    if let Some(memory) = memory {
        config.memory_mib = Some(memory);
    }
    if let Some(disk) = disk {
        config.disk_gib = disk;
    }
    if let Some(publish) = publish {
        config.publish = publish;
    }
    if let Some(gpu) = gpu {
        config.gpu = gpu.into();
    }
    if let Some(ane) = ane {
        config.ane = ane.into();
    }
    if let Some(mps) = mps {
        config.mps = mps.into();
    }
    if let Some(metal) = metal {
        config.metal = metal.into();
    }
    if let Some(video) = video {
        config.video = video.into();
    }
    if let Some(python) = torch_python {
        config.torch_python = if python == "auto" {
            String::new()
        } else {
            python
        };
    }
    Ok(changed)
}

fn configure(settings: Settings) -> anyhow::Result<std::process::ExitCode> {
    let mut config = config::Config::load()?;
    let disk = settings.disk;
    let changed = match apply(&mut config, settings) {
        Ok(changed) => changed,
        Err(e) => {
            eprintln!("lighter: {e}");
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    if let Some(disk) = disk {
        let image = paths::data_disk()?
            .metadata()
            .map(|m| m.len() >> 30)
            .unwrap_or(0);
        if image > disk {
            println!(
                "The disk is already {image} GiB and disks never shrink; it stays {image} GiB."
            );
        }
    }
    if changed {
        config.save()?;
        println!("Saved. Restart for it to take effect: `lighter restart`");
    }
    println!(
        "  resources  {}",
        match config.resources {
            config::Resources::Fixed => "fixed (a slice of the Mac)",
            config::Resources::Cooperative => "cooperative (the Mac's, shared, experimental)",
        }
    );
    let limit = |set: bool, unset: &'static str| match (config.resources, set) {
        (config::Resources::Cooperative, true) => " (limit)",
        (config::Resources::Cooperative, false) => unset,
        _ => "",
    };
    println!(
        "  cpus       {}{}",
        config.vcpus(),
        limit(config.cpus.is_some(), " (the Mac's)")
    );
    println!(
        "  memory     {} MiB{}",
        config.memory_mib(),
        limit(config.memory_mib.is_some(), " (twice the Mac's)")
    );
    println!("  disk       {} GiB", config.disk_gib);
    println!(
        "  publish    {}",
        match config.publish {
            config::Publish::Lan => "lan (every interface, as Docker does)",
            config::Publish::Localhost => "localhost (wildcard publishes on loopback)",
        }
    );
    println!("  gpu        {}", if config.gpu { "on" } else { "off" });
    println!("  ane        {}", if config.ane { "on" } else { "off" });
    println!("  metal      {}", if config.metal { "on" } else { "off" });
    println!("  video      {}", if config.video { "on" } else { "off" });
    println!(
        "  mps        {}{}",
        if config.mps { "on" } else { "off" },
        if config.torch_python.is_empty() {
            String::new()
        } else {
            format!(" (torch from {})", config.torch_python)
        }
    );
    for share in &config.shares {
        println!("  share      {share}");
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_is_applied() {
        let mut config = config::Config::default();
        let given = Settings {
            metal: Some(config::Toggle::Off),
            video: Some(config::Toggle::Off),
            gpu: Some(config::Toggle::Off),
            ..Settings::default()
        };
        assert_eq!(apply(&mut config, given), Ok(true));
        assert!(!config.metal && !config.video && !config.gpu);
        assert_eq!(apply(&mut config, Settings::default()), Ok(false));
    }

    #[test]
    fn a_share_must_be_a_folder() {
        let mut config = config::Config::default();
        let before = config.shares.clone();
        let given = Settings {
            share: vec!["/nowhere/at/all".into()],
            ..Settings::default()
        };
        assert!(
            apply(&mut config, given)
                .unwrap_err()
                .contains("not a folder")
        );
        assert_eq!(config.shares, before);
    }
}
