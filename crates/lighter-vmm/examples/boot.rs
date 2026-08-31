//! Boots a guest kernel and attaches the terminal to its console.
//!
//! This is the milestone-1 vehicle: `make gate-m1` drives it non-interactively
//! and checks the guest reached a shell, while running it by hand gives you a
//! real prompt.

use std::path::PathBuf;
use std::process::ExitCode;

use lighter_vmm::net::PortForward;
use lighter_vmm::virtio::fs::Share;
use lighter_vmm::{Machine, MachineConfig, StopReason};

fn parse_forward(spec: &str) -> Option<PortForward> {
    let (host, guest) = spec.split_once(':')?;
    Some(PortForward {
        host_port: host.parse().ok()?,
        guest_port: guest.parse().ok()?,
    })
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LIGHTER_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let mut config = MachineConfig::default();
    let mut forwards: Vec<PortForward> = Vec::new();
    let mut sockets: Vec<(PathBuf, u32)> = Vec::new();
    let mut docker_socket: Option<PathBuf> = None;
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
            "--net" => config.gvproxy = Some(PathBuf::from(args.next().unwrap_or_default())),
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
            "--forward" => {
                // host:guest, added once the machine is up.
                let spec = args.next().unwrap_or_default();
                match parse_forward(&spec) {
                    Some(f) => forwards.push(f),
                    None => {
                        eprintln!("--forward wants HOST:GUEST, got {spec:?}");
                        return ExitCode::from(2);
                    }
                }
            }
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

    for (path, guest_port) in &sockets {
        if let Err(e) = machine.proxy_socket(path, *guest_port) {
            eprintln!("lighter: cannot serve {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    }

    for forward in &forwards {
        // A forward the guest is not listening on yet is fine: gvproxy accepts
        // on the host side and only then dials the guest.
        if let Some(network) = machine.network()
            && let Err(e) = network.expose(*forward)
        {
            eprintln!("lighter: {e}");
            return ExitCode::FAILURE;
        }
    }

    if let Some(socket) = &docker_socket {
        match machine.network() {
            Some(network) => {
                if let Err(e) = lighter_docker::PortWatcher::start(socket, network.clone()) {
                    eprintln!("lighter: cannot watch docker ports: {e}");
                    return ExitCode::FAILURE;
                }
            }
            // Without a network there is nowhere to put a forward, and silently
            // doing nothing would look like the watcher working.
            None => {
                eprintln!("lighter: --docker-ports needs --net");
                return ExitCode::from(2);
            }
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
