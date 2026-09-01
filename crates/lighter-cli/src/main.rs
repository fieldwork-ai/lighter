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

mod config;
mod context;
mod doctor;
mod machine;
mod paths;
mod run;
mod service;

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
enum Command {
    /// Start the machine and point the Docker CLI at it.
    Start {
        /// How long to wait for Docker to answer.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Stop the machine.
    Stop,
    /// Restart the machine.
    Restart,
    /// Say whether it is running, and what it costs.
    Status,
    /// Check that this Mac can run lighter, and say what to fix if not.
    Doctor,
    /// Show the machine's log.
    Logs {
        /// Follow the log rather than printing what is there.
        #[arg(short, long)]
        follow: bool,
    },
    /// Show or change the configuration.
    Config {
        /// Cores to give the guest.
        #[arg(long)]
        cpus: Option<u32>,
        /// Memory ceiling, in MiB. The guest gives back what it does not use.
        #[arg(long)]
        memory: Option<u64>,
        /// Size of the disk images and volumes live on, in GiB.
        #[arg(long)]
        disk: Option<u64>,
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
    /// Become the machine. Not for typing; `start` runs this.
    #[command(hide = true)]
    Run,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
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
        Command::Run => {
            run::machine()?;
            Ok(std::process::ExitCode::SUCCESS)
        }
        Command::Start { timeout } => start(Duration::from_secs(timeout)),
        Command::Stop => stop(),
        Command::Restart => {
            stop()?;
            start(Duration::from_secs(120))
        }
        Command::Status => status(),
        Command::Doctor => {
            let findings = doctor::run();
            print!("{}", doctor::report(&findings));
            Ok(if findings.iter().all(|f| f.ok) {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::FAILURE
            })
        }
        Command::Logs { follow } => logs(follow),
        Command::Config { cpus, memory, disk } => configure(cpus, memory, disk),
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
    let config = config::Config::load()?;
    // Checked before starting rather than after failing: a missing kernel
    // produces a machine that exits immediately, and the log says less than
    // this does.
    let blocking: Vec<_> = doctor::run()
        .into_iter()
        .filter(|f| !f.ok && f.what != "docker context" && f.what != "machine")
        .collect();
    if !blocking.is_empty() {
        eprint!("{}", doctor::report(&blocking));
        anyhow::bail!("cannot start; see `lighter doctor`");
    }

    println!(
        "Starting lighter ({} cores, {} MiB)…",
        config.cpus, config.memory_mib
    );
    let pid = machine::start(&config, timeout)?;
    let socket = paths::docker_socket()?;
    let version = machine::docker_version(&socket)?;
    println!("Docker {version}");
    if paths::is_default_home() {
        context::install(&socket)?;
        println!("Running as pid {pid}; the docker CLI now points at it.");
    } else {
        println!(
            "Running as pid {pid}; custom home, context untouched — use DOCKER_HOST=unix://{}",
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
        let _ = context::select_default();
    }
    if machine::stop(Duration::from_secs(30))? {
        println!("Stopped.");
    } else {
        println!("Not running.");
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn status() -> anyhow::Result<std::process::ExitCode> {
    let status = machine::status()?;
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

fn configure(
    cpus: Option<u32>,
    memory: Option<u64>,
    disk: Option<u64>,
) -> anyhow::Result<std::process::ExitCode> {
    let mut config = config::Config::load()?;
    let changed = cpus.is_some() || memory.is_some() || disk.is_some();
    if let Some(cpus) = cpus {
        config.cpus = cpus;
    }
    if let Some(memory) = memory {
        config.memory_mib = memory;
    }
    if let Some(disk) = disk {
        config.disk_gib = disk;
    }
    if changed {
        config.save()?;
        println!("Saved. Restart for it to take effect: `lighter restart`");
    }
    println!("  cpus       {}", config.cpus);
    println!("  memory     {} MiB", config.memory_mib);
    println!("  disk       {} GiB", config.disk_gib);
    for share in &config.shares {
        println!("  share      {share}");
    }
    Ok(std::process::ExitCode::SUCCESS)
}
