//! Starting when you log in.
//!
//! A launchd agent, written to `~/Library/LaunchAgents`. A user agent rather
//! than a system daemon: lighter runs as you, shares your files, and has no
//! business with a privileged launchd context.

use std::path::PathBuf;

use crate::paths;

const LABEL: &str = "dev.lighter.machine";

fn plist_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

/// Writes the agent and loads it.
pub fn install() -> anyhow::Result<()> {
    // The bundled copy, for the same reason `lighter start` uses it: a
    // process launchd starts from the bundle carries a name and an icon.
    let exe = crate::bundle::ensure()?;
    let guest = paths::guest_dir()?;
    let gvproxy = paths::gvproxy()?;
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
        <key>LIGHTER_GVPROXY</key>
        <string>{gvproxy}</string>
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
        exe = exe.display(),
        guest = guest.display(),
        gvproxy = gvproxy.display(),
        log = log.display(),
    );
    std::fs::write(&path, plist)?;

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
