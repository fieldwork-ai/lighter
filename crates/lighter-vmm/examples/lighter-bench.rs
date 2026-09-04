//! Boots a guest kernel and attaches the terminal to its console.
//!
//! This is the milestone-1 vehicle: `make gate-m1` drives it non-interactively
//! and checks the guest reached a shell, while running it by hand gives you a
//! real prompt.

use std::path::PathBuf;
use std::process::ExitCode;

use lighter_vmm::virtio::fs::Share;
use lighter_vmm::{Machine, MachineConfig, StopReason};

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LIGHTER_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let mut config = MachineConfig::default();
    let mut sockets: Vec<(PathBuf, u32)> = Vec::new();
    let mut docker_socket: Option<PathBuf> = None;
    let mut report_memory = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--kernel" => config.kernel = PathBuf::from(args.next().unwrap_or_default()),
            "--initramfs" => {
                config.initramfs = Some(PathBuf::from(args.next().unwrap_or_default()))
            }
            "--cmdline" => config.cmdline = args.next().unwrap_or_default(),
            "--cpus" => config.vcpus = args.next().and_then(|v| v.parse().ok()).unwrap_or(1),
            "--memory-mib" => {
                let mib: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(2048);
                config.ram_bytes = mib << 20;
            }
            "--disk" => config
                .disks
                .push(PathBuf::from(args.next().unwrap_or_default())),
            "--disk-size-gib" => {
                let gib: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(64);
                config.disk_size_bytes = gib << 30;
            }
            "--no-tty" => config.interactive = false,
            // Logs the process's own physical footprint on an interval. The
            // memory gate has no other way to watch a number only this process
            // can see.
            "--report-memory" => report_memory = true,
            "--net" => config.network = true,
            "--vsock" => {
                // PATH:GUEST_PORT — a host socket carried to a guest port.
                let spec = args.next().unwrap_or_default();
                match spec
                    .rsplit_once(':')
                    .and_then(|(p, port)| port.parse().ok().map(|port| (PathBuf::from(p), port)))
                {
                    Some(pair) => sockets.push(pair),
                    None => {
                        eprintln!("--vsock wants PATH:GUEST_PORT, got {spec:?}");
                        return ExitCode::from(2);
                    }
                }
            }
            "--run-dir" => config.run_dir = PathBuf::from(args.next().unwrap_or_default()),
            "--share" => {
                // TAG:PATH — a host directory the guest mounts by TAG.
                let spec = args.next().unwrap_or_default();
                match spec.split_once(':') {
                    Some((tag, path)) if !tag.is_empty() && !path.is_empty() => {
                        config.shares.push(Share {
                            tag: tag.to_string(),
                            path: PathBuf::from(path),
                        })
                    }
                    _ => {
                        eprintln!("--share wants TAG:PATH, got {spec:?}");
                        return ExitCode::from(2);
                    }
                }
            }
            // Watch the guest's Docker daemon and mirror whatever it publishes
            // onto the host, for as long as the machine runs.
            "--docker-ports" => docker_socket = args.next().map(PathBuf::from),
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let mut machine = match Machine::start(&config) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("lighter: {e}");
            return ExitCode::FAILURE;
        }
    };
    if config.network
        && let Err(e) = lighter_vmm::streams::start(machine.vsock())
    {
        eprintln!("lighter: cannot start streams: {e}");
        return ExitCode::FAILURE;
    }

    if report_memory {
        // Footprint and what the guest has handed back, together: the first
        // alone cannot tell "the guest is not reporting" from "the guest
        // reported and macOS kept the pages anyway".
        let balloon = machine.balloon().clone();
        std::thread::Builder::new()
            .name("memory-report".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    tracing::info!(
                        mib = lighter_vmm::footprint::bytes() / (1 << 20),
                        offered_mib = balloon.offered_bytes() / (1 << 20),
                        reported_mib = balloon.reported_bytes() / (1 << 20),
                        ballooned_mib = balloon.ballooned_bytes() / (1 << 20),
                        "FOOTPRINT"
                    );
                }
            })
            .expect("failed to spawn the memory reporter");
    }

    for (path, guest_port) in &sockets {
        if let Err(e) = machine.proxy_socket(path, *guest_port) {
            eprintln!("lighter: cannot serve {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    }

    if let Some(socket) = &docker_socket {
        // Without a network there are no streams to carry a published port,
        // and silently doing nothing would look like the watcher working.
        if !config.network {
            eprintln!("lighter: --docker-ports needs --net");
            return ExitCode::from(2);
        }
        let mapper = lighter_vmm::streams::PortMapper::new(machine.vsock());
        if let Err(e) = lighter_docker::PortWatcher::start(socket, mapper) {
            eprintln!("lighter: cannot watch docker ports: {e}");
            return ExitCode::FAILURE;
        }
    }

    match machine.wait() {
        Ok(StopReason::SystemOff) => {
            eprintln!("\nlighter: guest powered off");
            ExitCode::SUCCESS
        }
        Ok(reason) => {
            eprintln!("\nlighter: guest stopped: {reason:?}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nlighter: {e}");
            ExitCode::FAILURE
        }
    }
}
