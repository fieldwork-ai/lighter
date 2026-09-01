//! Where lighter keeps its things.
//!
//! One directory, `~/.lighter`, and nothing outside it except the Docker
//! context — which is Docker's own file and is registered rather than written
//! by hand. Uninstalling is therefore deleting one directory and one context,
//! and there is nowhere else to look when something is stale.

use std::path::{Path, PathBuf};

/// The root of everything lighter owns.
pub fn home() -> anyhow::Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("LIGHTER_HOME") {
        return Ok(PathBuf::from(explicit));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("HOME is not set, so there is nowhere to keep state"))?;
    Ok(Path::new(&home).join(".lighter"))
}

/// The socket the Docker CLI talks to.
pub fn docker_socket() -> anyhow::Result<PathBuf> {
    Ok(home()?.join("docker.sock"))
}

/// The running machine's process id.
pub fn pid_file() -> anyhow::Result<PathBuf> {
    Ok(home()?.join("lighter.pid"))
}

pub fn log_file() -> anyhow::Result<PathBuf> {
    Ok(home()?.join("machine.log"))
}

/// Docker's storage: images, containers, named volumes.
pub fn data_disk() -> anyhow::Result<PathBuf> {
    Ok(home()?.join("data.img"))
}

pub fn config_file() -> anyhow::Result<PathBuf> {
    Ok(home()?.join("config.json"))
}

/// Where the guest kernel and root filesystem live.
///
/// Beside the binary when installed, and in the repository when run from a
/// checkout — so `cargo run` and an installed copy both work without a flag
/// or an environment variable to remember.
pub fn guest_dir() -> anyhow::Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("LIGHTER_GUEST_DIR") {
        return Ok(PathBuf::from(explicit));
    }
    let exe = std::env::current_exe()?;
    let beside = exe
        .parent()
        .map(|dir| dir.join("../share/lighter"))
        .filter(|dir| dir.join("Image").exists());
    if let Some(installed) = beside {
        return Ok(installed);
    }
    // A checkout: target/{debug,release}/lighter -> the repository root.
    let repo = exe
        .ancestors()
        .find(|dir| dir.join("guest/out/Image").exists())
        .map(|dir| dir.join("guest/out"));
    repo.ok_or_else(|| {
        anyhow::anyhow!(
            "cannot find the guest image; build it with `make guest` or set LIGHTER_GUEST_DIR"
        )
    })
}

pub fn kernel() -> anyhow::Result<PathBuf> {
    Ok(guest_dir()?.join("Image"))
}

pub fn rootfs() -> anyhow::Result<PathBuf> {
    Ok(guest_dir()?.join("rootfs.ext4"))
}

/// The network sidecar, looked for beside the binary and then in the checkout.
pub fn gvproxy() -> anyhow::Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("LIGHTER_GVPROXY") {
        return Ok(PathBuf::from(explicit));
    }
    let exe = std::env::current_exe()?;
    if let Some(beside) = exe.parent().map(|dir| dir.join("gvproxy"))
        && beside.exists()
    {
        return Ok(beside);
    }
    let repo = exe
        .ancestors()
        .find(|dir| dir.join("vendor/gvproxy").exists())
        .map(|dir| dir.join("vendor/gvproxy"));
    repo.ok_or_else(|| anyhow::anyhow!("cannot find gvproxy; run scripts/fetch-gvproxy.sh"))
}
