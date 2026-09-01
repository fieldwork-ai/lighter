//! The app bundle the machine runs from, so it has a face.
//!
//! Activity Monitor shows a name and an icon only for processes whose
//! executable lives inside an app bundle — a bare CLI is a generic terminal
//! silhouette named after its file. The machine deserves better: `lighter`
//! with the flame, findable at a glance by whoever is wondering what is
//! using two gigabytes.
//!
//! So the CLI maintains a minimal bundle inside its own home —
//! `~/.lighter/lighter.app` — holding a copy of itself, an Info.plist, and
//! the icon, and the machine process is spawned from that copy. The bundle
//! is refreshed whenever the CLI's bytes differ, so an upgrade reaches it on
//! the next start. Nothing else changes: same binary, same arguments, and
//! `LIGHTER_GUEST_DIR` is passed explicitly because the copy cannot find the
//! guest image by walking up from its own path the way a checkout build can.

use std::path::PathBuf;

const ICON: &[u8] = include_bytes!("../../../assets/lighter.icns");

const INFO_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>lighter</string>
    <key>CFBundleDisplayName</key>
    <string>lighter</string>
    <key>CFBundleIdentifier</key>
    <string>dev.lighter.machine</string>
    <key>CFBundleExecutable</key>
    <string>lighter</string>
    <key>CFBundleIconFile</key>
    <string>lighter</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>
"#;

/// Builds or refreshes the bundle, returning the path of the executable
/// inside it.
pub fn ensure() -> anyhow::Result<PathBuf> {
    let home = crate::paths::home()?;
    let contents = home.join("lighter.app/Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    std::fs::create_dir_all(&macos)?;
    std::fs::create_dir_all(&resources)?;

    let plist = contents.join("Info.plist");
    if std::fs::read(&plist).ok().as_deref() != Some(INFO_PLIST.as_bytes()) {
        std::fs::write(&plist, INFO_PLIST)?;
    }
    let icon = resources.join("lighter.icns");
    if std::fs::read(&icon).ok().map(|b| b.len()) != Some(ICON.len()) {
        std::fs::write(&icon, ICON)?;
    }

    let source = std::env::current_exe()?;
    let target = macos.join("lighter");
    let stale = match (std::fs::metadata(&source), std::fs::metadata(&target)) {
        (Ok(s), Ok(t)) => s.len() != t.len() || s.modified()? > t.modified()?,
        _ => true,
    };
    if stale {
        // Copy to a temporary name and rename over: the running machine may
        // be executing the old copy, and overwriting a mapped binary in
        // place is how processes crash mysteriously later.
        let staging = macos.join(".lighter.next");
        std::fs::copy(&source, &staging)?;
        std::fs::rename(&staging, &target)?;
    }
    Ok(target)
}
