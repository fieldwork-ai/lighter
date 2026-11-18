//! lighter's root helper: installing it, removing it, and saying whether
//! it is there (`lighter helper install | uninstall | status`).
//!
//! The helper binds what macOS keeps from a process that is not root: a
//! published port below 1024 on one address (`lighter_helper`). Nothing
//! else needs it, so it is opt-in: a machine without it forwards every
//! other port, and `lighter doctor` names the ones waiting for it.

use std::path::{Path, PathBuf};

use lighter_helper::{INSTALLED, LABEL, LOG, PLIST};

/// What an installed helper must be: Fieldwork's, notarized.
const REQUIREMENT: &str = "anchor apple generic and identifier \"dev.lighter.helper\" and certificate leaf[subject.OU] = \"N7N6BNF95K\"";

/// The helper of 0.12 to 0.13.10, which bridged LAN mode for builds without
/// `com.apple.vm.networking`: a release has bridged by itself since 0.12.5,
/// and the helper no longer does. Installing or removing the new one removes
/// it.
mod old {
    pub const LABEL: &str = "dev.lighter.bridge";
    pub const FILES: [&str; 4] = [
        "/Library/LaunchDaemons/dev.lighter.bridge.plist",
        "/Library/PrivilegedHelperTools/dev.lighter.bridge",
        "/var/run/dev.lighter.bridge.sock",
        "/var/log/dev.lighter.bridge.log",
    ];
}

/// Whether the helper is installed.
pub fn installed() -> bool {
    Path::new(PLIST).exists() && Path::new(INSTALLED).exists()
}

/// Whether the old bridging helper is still installed.
pub fn old_installed() -> bool {
    Path::new(old::FILES[0]).exists()
}

/// `sudo lighter helper install`: installs the helper as a launchd daemon
/// that serves the user who ran sudo (and any earlier ones).
pub fn install() -> anyhow::Result<std::process::ExitCode> {
    // SAFETY: plain geteuid.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("lighter: run it with sudo: `sudo lighter helper install`");
        return Ok(std::process::ExitCode::FAILURE);
    }
    let uid: u32 = match std::env::var("SUDO_UID").ok().and_then(|u| u.parse().ok()) {
        Some(uid) if uid != 0 => uid,
        _ => {
            eprintln!(
                "lighter: run it with sudo from the account that uses lighter, so the helper knows whom to serve"
            );
            return Ok(std::process::ExitCode::FAILURE);
        }
    };
    let source = bundled_helper()
        .ok_or_else(|| anyhow::anyhow!("this installation has no lighter-helper beside it"))?;
    // The copy is what is checked, in a root-owned directory: checked in the
    // user's own installation, the file could be replaced between the
    // check and the copy, and the replacement installed as root.
    std::fs::create_dir_all("/Library/PrivilegedHelperTools")?;
    let staging = format!("{INSTALLED}.new");
    let _ = std::fs::remove_file(&staging);
    std::fs::copy(&source, &staging)?;
    use std::os::unix::fs::PermissionsExt;
    let c = std::ffi::CString::new(staging.as_str())?;
    // SAFETY: chown of a path we just wrote, to root:wheel.
    unsafe { libc::chown(c.as_ptr(), 0, 0) };
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
    let verified = std::process::Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", &format!("-R={REQUIREMENT}")])
        .arg(&staging)
        .stderr(std::process::Stdio::null())
        .status()?;
    if !verified.success() {
        let _ = std::fs::remove_file(&staging);
        eprintln!(
            "lighter: {} is not signed by Fieldwork, so it is not installed as root. A build of your own runs it by hand: `sudo {} --socket /tmp/lighter-helper.sock --allow-uid {uid} --any-client`, with LIGHTER_HELPER_SOCKET=/tmp/lighter-helper.sock in the machine's environment",
            source.display(),
            source.display()
        );
        return Ok(std::process::ExitCode::FAILURE);
    }
    let removed_old = remove_old()?;
    let mut uids = allowed_uids();
    if !uids.contains(&uid) {
        uids.push(uid);
    }
    bootout(LABEL);
    std::fs::rename(&staging, INSTALLED)?;
    std::fs::write(PLIST, plist(&uids))?;
    std::fs::set_permissions(PLIST, std::fs::Permissions::from_mode(0o644))?;
    if !bootstrap() {
        // Not left half installed: `status` reads the plist as installed.
        let _ = remove(&[PLIST]);
        anyhow::bail!("launchctl could not load {PLIST}");
    }
    if removed_old {
        println!(
            "Removed the network helper of lighter 0.13.10 and earlier; LAN mode no longer uses it."
        );
    }
    println!(
        "lighter's helper is installed, for user {}.",
        uids.iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "A running machine opens ports below 1024 on one address within half a minute; no restart."
    );
    Ok(std::process::ExitCode::SUCCESS)
}

/// `sudo lighter helper uninstall`: removes the helper, and the old one.
pub fn uninstall() -> anyhow::Result<std::process::ExitCode> {
    // SAFETY: plain geteuid.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("lighter: run it with sudo: `sudo lighter helper uninstall`");
        return Ok(std::process::ExitCode::FAILURE);
    }
    bootout(LABEL);
    remove(&[PLIST, INSTALLED, lighter_helper::SOCKET])?;
    remove_old()?;
    println!(
        "lighter's helper is removed. Ports below 1024 published on one address stay closed until it is installed again."
    );
    Ok(std::process::ExitCode::SUCCESS)
}

/// `lighter helper status`.
pub fn status() -> anyhow::Result<std::process::ExitCode> {
    if installed() {
        println!(
            "  helper     installed ({INSTALLED}), serving user {}",
            allowed_uids()
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    } else {
        println!(
            "  helper     not installed: ports below 1024 published on one address (127.0.0.1:80) stay closed; `sudo lighter helper install`"
        );
    }
    if old_installed() {
        println!(
            "  old        the network helper of lighter 0.13.10 and earlier is still installed; `sudo lighter helper uninstall` removes it"
        );
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// Unloads a launchd job and waits until launchd has let it go: bootout
/// can return first, and a bootstrap of the same label straight after then
/// fails ("Input/output error"). One that is not loaded is not an error,
/// and launchctl's saying so on stderr is not shown.
fn bootout(label: &str) {
    let target = format!("system/{label}");
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["bootout", &target])
        .stderr(std::process::Stdio::null())
        .status();
    for _ in 0..50 {
        let loaded = std::process::Command::new("/bin/launchctl")
            .args(["print", &target])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !loaded {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Loads the helper's job, a few times: launchd can still be tearing down
/// the job it replaces.
fn bootstrap() -> bool {
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let loaded = std::process::Command::new("/bin/launchctl")
            .args(["bootstrap", "system", PLIST])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if loaded {
            return true;
        }
    }
    false
}

/// Removes files that may already be gone.
fn remove(paths: &[&str]) -> std::io::Result<()> {
    for path in paths {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Removes the old bridging helper, saying whether it was there.
fn remove_old() -> std::io::Result<bool> {
    let was = old_installed();
    bootout(old::LABEL);
    remove(&old::FILES)?;
    Ok(was)
}

/// The helper shipped with this lighter: inside `lighter.app`, whose
/// signature covers it, beside the app's own binary or reached from the
/// installation's `bin`; beside the binary in a checkout.
fn bundled_helper() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let dir = exe.parent()?;
    [
        dir.join("lighter-helper"),
        dir.join("../share/lighter/lighter.app/Contents/MacOS/lighter-helper"),
    ]
    .into_iter()
    .find(|p| p.exists())
}

/// The users the installed helper serves, from its plist.
fn allowed_uids() -> Vec<u32> {
    let Ok(text) = std::fs::read_to_string(PLIST) else {
        return Vec::new();
    };
    uids_in(&text)
}

fn uids_in(plist: &str) -> Vec<u32> {
    let strings: Vec<&str> = plist
        .split("<string>")
        .skip(1)
        .filter_map(|s| s.split("</string>").next())
        .collect();
    strings
        .windows(2)
        .filter(|w| w[0] == "--allow-uid")
        .filter_map(|w| w[1].parse().ok())
        .collect()
}

fn plist(uids: &[u32]) -> String {
    let args: String = uids
        .iter()
        .map(|u| format!("\n\t\t<string>--allow-uid</string>\n\t\t<string>{u}</string>"))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{INSTALLED}</string>{args}
	</array>
	<key>Sockets</key>
	<dict>
		<key>Listeners</key>
		<dict>
			<key>SockPathName</key>
			<string>{socket}</string>
			<key>SockPathMode</key>
			<integer>438</integer>
		</dict>
	</dict>
	<key>StandardErrorPath</key>
	<string>{LOG}</string>
</dict>
</plist>
"#,
        socket = lighter_helper::SOCKET
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_names_its_users_and_the_socket() {
        let text = plist(&[501, 502]);
        assert_eq!(uids_in(&text), vec![501, 502]);
        assert!(
            text.contains("<integer>438</integer>"),
            "0666: the helper checks who connects itself"
        );
        assert!(text.contains(lighter_helper::SOCKET));
        assert!(text.contains(LABEL));
        assert!(!text.contains("--any-client"));
    }

    /// The old helper's files are all its own, so removing them cannot take
    /// the new one's.
    #[test]
    fn the_old_helper_is_not_the_new_one() {
        for file in old::FILES {
            assert!(file.contains("dev.lighter.bridge"), "{file}");
        }
        assert_ne!(old::LABEL, LABEL);
    }
}
