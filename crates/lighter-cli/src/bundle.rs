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

use std::path::{Path, PathBuf};

const ICON: &[u8] = include_bytes!("../../../assets/lighter.icns");

const INFO_PLIST: &str = include_str!("../../../assets/Info.plist");

/// Builds or refreshes the bundle, returning the path of the executable
/// inside it.
pub fn ensure() -> anyhow::Result<PathBuf> {
    // A release ships the bundle beside the kernel and the rootfs, signed
    // with the same Developer ID as the binary and notarized with it, and
    // that one is used as it is. Gatekeeper assesses an app bundle at its
    // first launch and puts its verdict to the user when it cannot verify
    // the developer, which for a bundle made here and signed ad hoc is
    // every time: a dialog on a Mac with someone at it, and a machine
    // process held in `_dyld_start` forever on one whose screen is locked.
    // A checkout has no such bundle and builds one, ad hoc, below.
    if let Ok(guest) = crate::paths::guest_dir() {
        let shipped = guest.join("lighter.app/Contents/MacOS/lighter");
        if shipped.exists() {
            return Ok(shipped);
        }
    }
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
        sign(&source, contents.parent().expect("Contents has a parent"))?;
    }
    Ok(target)
}

/// Signs the bundle, ad hoc, with the entitlements the source binary was
/// signed with.
///
/// A Developer ID copy inside a bundle whose `Info.plist` is not sealed by
/// any signature is what Gatekeeper calls damaged: the first `lighter
/// start` from a release install put up "lighter is damaged and can't be
/// opened" on a Mac with a screen and sat blocked forever on one without.
/// A checkout build is ad hoc already and was never affected. Signing the
/// bundle as a whole seals the plist and re-signs the copy, and ad hoc is
/// all a local copy needs: the entitlement is what makes the hypervisor
/// answer, and it is carried over from the source rather than looked up on
/// disk, so no layout — checkout, installer, Homebrew — has to be known.
fn sign(source: &Path, bundle: &Path) -> anyhow::Result<()> {
    let entitlements = std::process::Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", ":-"])
        .arg(source)
        .output()?;
    let mut command = std::process::Command::new("/usr/bin/codesign");
    command.args(["--force", "--sign", "-", "--options", "runtime"]);
    // Beside the bundle, never inside it: codesign seals everything under
    // Contents and refuses a stray file there as an unsealed subcomponent.
    let plist = bundle.with_extension("entitlements.plist");
    if entitlements.status.success() && !entitlements.stdout.is_empty() {
        std::fs::write(&plist, &entitlements.stdout)?;
        command.arg("--entitlements").arg(&plist);
    }
    let status = command.arg(bundle).status()?;
    let _ = std::fs::remove_file(&plist);
    anyhow::ensure!(status.success(), "codesign failed on {}", bundle.display());
    Ok(())
}
