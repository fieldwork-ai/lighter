//! Starting when you log in.
//!
//! A launchd agent, written to `~/Library/LaunchAgents`. A user agent rather
//! than a system daemon: lighter runs as you, shares your files, and has no
//! business with a privileged launchd context.

use std::path::PathBuf;

use crate::paths;

const LABEL: &str = "dev.lighter.machine";

pub fn plist_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

/// Writes the agent and loads it.
pub fn install() -> anyhow::Result<()> {
    // The bundled copy, for the same reason `lighter start` uses it: a
    // process launchd starts from the bundle carries a name and an icon.
    let mut exe = crate::bundle::ensure()?;
    let mut guest = paths::guest_dir()?;
    if let Some(i) = crate::installation::current()?
        && i.ownership.method == crate::installation::Method::Script
    {
        guest = i.ownership.prefix.join("current/share/lighter");
        exe = guest.join("lighter.app/Contents/MacOS/lighter");
    }
    register(&exe, &guest, true)
}

pub fn refresh_at(root: &std::path::Path, start: bool) -> anyhow::Result<()> {
    let mut guest = root.join("share/lighter");
    if let Some(i) = crate::installation::detect(&root.join("bin/lighter"))? {
        guest = match i.ownership.method {
            crate::installation::Method::Script => i.ownership.prefix.join("current/share/lighter"),
            crate::installation::Method::Homebrew => i
                .ownership
                .prefix
                .parent()
                .and_then(|p| p.parent())
                .ok_or_else(|| anyhow::anyhow!("invalid Brew prefix"))?
                .join("opt/lighter/share/lighter"),
        };
    }
    register(
        &guest.join("lighter.app/Contents/MacOS/lighter"),
        &guest,
        start,
    )
}

fn register(exe: &std::path::Path, guest: &std::path::Path, start: bool) -> anyhow::Result<()> {
    let log = paths::log_file()?;
    let path = plist_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // `KeepAlive` with `SuccessfulExit: false` is the crash recovery: launchd
    // restarts a machine that died, and does not restart one that was asked to
    // stop. Without the distinction, `lighter stop` would be a thing that
    // pauses for a second.
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>run</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>LIGHTER_GUEST_DIR</key>
        <string>{guest}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        exe = crate::updates::xml(&exe.to_string_lossy()),
        guest = crate::updates::xml(&guest.to_string_lossy()),
        log = crate::updates::xml(&log.to_string_lossy()),
    );
    std::fs::write(&path, &plist)?;
    // KeepAlive/SuccessfulExit implies RunAtLoad. Merely setting RunAtLoad
    // false still boots the VM. Preserve login registration on disk and leave
    // it unloaded until an explicit start or the next login.
    if !start {
        suspend()?;
        return Ok(());
    }

    // `bootout` first, so installing over an older agent replaces it rather
    // than failing with "service already loaded".
    let target = format!("gui/{}", user_id());
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["bootout", &format!("{target}/{LABEL}")])
        .output();
    let output = std::process::Command::new("/bin/launchctl")
        .arg("bootstrap")
        .arg(&target)
        .arg(&path)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

pub fn suspend() -> anyhow::Result<()> {
    let target = format!("gui/{}/{LABEL}", user_id());
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["bootout", &target])
        .output()?;
    let check = std::process::Command::new("/bin/launchctl")
        .args(["print", &target])
        .output()?;
    anyhow::ensure!(
        !check.status.success(),
        "launchd did not unload the running service"
    );
    Ok(())
}

pub fn configured_executable() -> anyhow::Result<Option<PathBuf>> {
    let path = plist_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let out = std::process::Command::new("/usr/bin/plutil")
        .args(["-extract", "ProgramArguments", "json", "-o", "-"])
        .arg(path)
        .output()?;
    anyhow::ensure!(out.status.success(), "cannot read login service executable");
    let args: Vec<String> = serde_json::from_slice(&out.stdout)?;
    Ok(args.first().map(PathBuf::from))
}

pub fn start_registered() -> anyhow::Result<bool> {
    if !paths::is_default_home() {
        return Ok(false);
    }
    let Some(i) = crate::installation::current()? else {
        return Ok(false);
    };
    let Some(exe) = configured_executable()? else {
        return Ok(false);
    };
    let brew_opt = i
        .ownership
        .prefix
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("opt/lighter"));
    if !exe.starts_with(&i.ownership.prefix)
        && !(i.ownership.method == crate::installation::Method::Homebrew
            && brew_opt.as_ref().is_some_and(|p| exe.starts_with(p)))
    {
        return Ok(false);
    }
    if exe.canonicalize().ok()
        != i.root
            .join("share/lighter/lighter.app/Contents/MacOS/lighter")
            .canonicalize()
            .ok()
    {
        refresh_at(&i.root, true)?;
        return Ok(true);
    }
    let target = format!("gui/{}/{LABEL}", user_id());
    let loaded = std::process::Command::new("/bin/launchctl")
        .args(["print", &target])
        .output()?
        .status
        .success();
    if loaded {
        let out = std::process::Command::new("/bin/launchctl")
            .args(["kickstart", &target])
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "could not start registered service: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    } else {
        refresh_at(&i.root, true)?;
    }
    Ok(true)
}

/// Unloads the agent and removes it.
pub fn uninstall() -> anyhow::Result<()> {
    let path = plist_path()?;
    let target = format!("gui/{}/{LABEL}", user_id());
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["bootout", &target])
        .output();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

fn user_id() -> u32 {
    // SAFETY: takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}
