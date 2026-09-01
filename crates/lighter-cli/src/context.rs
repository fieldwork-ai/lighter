//! Telling the Docker CLI where to find us.
//!
//! A Docker context is a named endpoint, and registering one is what makes
//! `docker ps` work with nothing exported and nothing to remember. It is done
//! through the `docker` CLI rather than by writing its metadata directory
//! directly: that layout is Docker's to change, and a tool that wrote it by
//! hand would be broken by an upgrade with no warning.

/// The context lighter registers.
pub const NAME: &str = "lighter";

/// Registers or updates the context, and selects it.
pub fn install(socket: &std::path::Path) -> anyhow::Result<()> {
    let endpoint = format!("host=unix://{}", socket.display());
    let exists = list()?.iter().any(|name| name == NAME);
    let verb = if exists { "update" } else { "create" };
    let output = std::process::Command::new("docker")
        .args(["context", verb, NAME, "--docker", &endpoint])
        .arg("--description")
        .arg("lighter")
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "docker context {verb} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let output = std::process::Command::new("docker")
        .args(["context", "use", NAME])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "docker context use failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Selects a different context, so `docker` does not point at a socket that is
/// no longer there.
pub fn select_default() -> anyhow::Result<()> {
    let _ = std::process::Command::new("docker")
        .args(["context", "use", "default"])
        .output()?;
    Ok(())
}

/// The context the CLI is currently pointed at.
pub fn current() -> anyhow::Result<Option<String>> {
    let output = std::process::Command::new("docker")
        .args(["context", "show"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok((!name.is_empty()).then_some(name))
        }
        _ => Ok(None),
    }
}

fn list() -> anyhow::Result<Vec<String>> {
    let output = std::process::Command::new("docker")
        .args(["context", "ls", "--format", "{{.Name}}"])
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()),
        _ => Ok(Vec::new()),
    }
}
