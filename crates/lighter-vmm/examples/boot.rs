//! Boots a guest kernel and attaches the terminal to its console.
//!
//! This is the milestone-1 vehicle: `make gate-m1` drives it non-interactively
//! and checks the guest reached a shell, while running it by hand gives you a
//! real prompt.

use std::path::PathBuf;
use std::process::ExitCode;

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
            "--no-tty" => config.interactive = false,
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let machine = match Machine::start(&config) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("lighter: {e}");
            return ExitCode::FAILURE;
        }
    };

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
