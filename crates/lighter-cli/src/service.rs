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
    anyhow::ensure!(
        paths::is_default_home(),
        "login registration is only supported for the default VM home"
    );
    // A packaged release registers the path its installer keeps stable, so
    // the next upgrade does not leave the agent naming a release that is gone.
    if let Some(i) = crate::installation::current()? {
        return refresh_at(&i.root, true);
    }
    // A development build: the bundled copy, for the same reason `lighter
    // start` uses it: a process launchd starts from the bundle carries a name
    // and an icon.
    register(&crate::bundle::ensure()?, &paths::guest_dir()?, true)
}

pub fn refresh_at(root: &std::path::Path, start: bool) -> anyhow::Result<()> {
    let (exe, guest) = stable_paths(root)?;
    register(&exe, &guest, start)
}

/// The executable and guest directory a login agent should name for the
/// release at `root`: ones its installer keeps in place across upgrades.
/// Homebrew replaces the keg (`Cellar/lighter/<version>`) on every upgrade
/// and deletes the old one, but keeps `opt/lighter` pointing at the current;
/// the install script keeps `current`.
fn stable_paths(
    root: &std::path::Path,
) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
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
    Ok((guest.join("lighter.app/Contents/MacOS/lighter"), guest))
}

/// Points an existing login agent that names a Homebrew keg at `opt`
/// instead. Before 0.11.1 `lighter install` registered the keg itself, which
/// the next `brew upgrade` deletes, and launchd then starts nothing at login.
/// Only the file changes: the machine launchd is running now keeps running,
/// and the next login (or `lighter restart`) starts the upgraded release.
pub fn heal() -> anyhow::Result<()> {
    if !paths::is_default_home() {
        return Ok(());
    }
    let Some(i) = crate::installation::current()? else {
        return Ok(());
    };
    if i.ownership.method != crate::installation::Method::Homebrew {
        return Ok(());
    }
    let Some(configured) = configured_executable()? else {
        return Ok(());
    };
    let (exe, guest) = stable_paths(&i.root)?;
    if names_a_keg(&configured, &i.ownership.prefix, &exe) {
        write_plist(&exe, &guest)?;
    }
    Ok(())
}

/// Whether an agent naming `configured` names one of the kegs under
/// `cellar` rather than the stable `exe`. A development build's agent names
/// neither, and is left alone.
fn names_a_keg(
    configured: &std::path::Path,
    cellar: &std::path::Path,
    exe: &std::path::Path,
) -> bool {
    configured != exe && configured.starts_with(cellar)
}

fn register(exe: &std::path::Path, guest: &std::path::Path, start: bool) -> anyhow::Result<()> {
    let path = write_plist(exe, guest)?;
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

/// Writes the agent's plist, and nothing else: launchd reads it at the next
/// load.
fn write_plist(exe: &std::path::Path, guest: &std::path::Path) -> anyhow::Result<PathBuf> {
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
        <key>HOME</key>
        <string>{home}</string>
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
        home = crate::updates::xml(&std::env::var("HOME")?),
        exe = crate::updates::xml(&exe.to_string_lossy()),
        guest = crate::updates::xml(&guest.to_string_lossy()),
        log = crate::updates::xml(&log.to_string_lossy()),
    );
    std::fs::write(&path, &plist)?;
    Ok(path)
}

pub fn suspend() -> anyhow::Result<()> {
    let target = format!("gui/{}/{LABEL}", user_id());
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["bootout", &target])
        .output()?;
    // bootout can return while launchd is still reaping the gracefully
    // stopping VM. Do not mistake that transient registration for failure.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let check = std::process::Command::new("/bin/launchctl")
            .args(["print", &target])
            .output()?;
        if !check.status.success() {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "launchd did not unload the running service within 30 seconds"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
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
    let print = std::process::Command::new("/bin/launchctl")
        .args(["print", &target])
        .output()?;
    // launchd runs the definition it loaded, not the file: an agent `heal`
    // rewrote since login is still loaded naming the old keg, which kickstart
    // would try to run. Load the file again instead.
    let loaded = print.status.success()
        && loaded_program(&String::from_utf8_lossy(&print.stdout)).as_deref()
            == Some(exe.as_path());
    if loaded {
        let out = std::process::Command::new("/bin/launchctl")
            // `-k`: the caller has established that no machine owns the home,
            // but launchd may not yet have reaped the one that just exited (a
            // `restart` gets here within milliseconds), and a plain kickstart
            // of a job it still counts as running does nothing. It then sees
            // a clean exit, which `KeepAlive: SuccessfulExit=false` does not
            // restart, and the machine stays down.
            .args(["kickstart", "-k", &target])
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

/// The program a `launchctl print` of a loaded job names.
fn loaded_program(print: &str) -> Option<PathBuf> {
    print
        .lines()
        .find_map(|l| l.trim().strip_prefix("program = "))
        .map(PathBuf::from)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// `<tmp>/Cellar/lighter/<version>`, laid out as Homebrew installs it.
    fn keg(tmp: &Path, version: &str) -> PathBuf {
        let root = tmp.join("Cellar/lighter").join(version);
        let app = root.join("share/lighter/lighter.app/Contents/MacOS");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(app.join("lighter"), b"").unwrap();
        std::fs::write(root.join("bin/lighter"), b"").unwrap();
        std::fs::write(root.join("INSTALL_RECEIPT.json"), b"{}").unwrap();
        root.canonicalize().unwrap()
    }

    #[test]
    fn a_homebrew_agent_names_opt_not_the_keg() {
        let tmp = tempfile::tempdir().unwrap();
        let root = keg(tmp.path(), "0.11.1");
        let (exe, guest) = stable_paths(&root).unwrap();
        let base = tmp.path().canonicalize().unwrap();
        assert_eq!(guest, base.join("opt/lighter/share/lighter"));
        assert_eq!(
            exe,
            base.join("opt/lighter/share/lighter/lighter.app/Contents/MacOS/lighter")
        );
    }

    #[test]
    fn the_loaded_program_is_read_from_launchctl() {
        let print = "gui/501/dev.lighter.machine = {\n\tactive count = 1\n\tpath = /Users/me/Library/LaunchAgents/dev.lighter.machine.plist\n\ttype = LaunchAgent\n\tstate = running\n\n\tprogram = /opt/homebrew/Cellar/lighter/0.11.0/bin/../share/lighter/lighter.app/Contents/MacOS/lighter\n\targuments = {\n";
        assert_eq!(
            loaded_program(print),
            Some(PathBuf::from(
                "/opt/homebrew/Cellar/lighter/0.11.0/bin/../share/lighter/lighter.app/Contents/MacOS/lighter"
            ))
        );
        assert_eq!(loaded_program("nothing loaded"), None);
    }

    #[test]
    fn only_an_agent_naming_a_keg_is_healed() {
        let cellar = Path::new("/opt/homebrew/Cellar/lighter");
        let exe =
            Path::new("/opt/homebrew/opt/lighter/share/lighter/lighter.app/Contents/MacOS/lighter");
        // What `lighter install` wrote before 0.11.1, as found on a Mac.
        assert!(names_a_keg(
            Path::new(
                "/opt/homebrew/Cellar/lighter/0.10.3/bin/../share/lighter/lighter.app/Contents/MacOS/lighter"
            ),
            cellar,
            exe
        ));
        assert!(!names_a_keg(exe, cellar, exe));
        assert!(!names_a_keg(
            Path::new("/Users/me/lighter/target/release/lighter.app/Contents/MacOS/lighter"),
            cellar,
            exe
        ));
    }
}
