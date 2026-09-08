//! Explicit activation, serialized independently of the download worker.
use crate::{
    installation::{self, Method},
    release, updates,
};
use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, symlink},
    },
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[derive(Serialize, Deserialize)]
struct Journal {
    previous: Option<PathBuf>,
    next: PathBuf,
    running: bool,
    login: bool,
}

/// A short shared lease prevents a start racing the release switch. A running
/// VM does not hold this lease; it owns immutable artifacts in its generation.
pub struct SelectionLease {
    _file: File,
}
impl Drop for SelectionLease {
    fn drop(&mut self) {
        // Explicit unlock also releases a descriptor inherited by a child
        // between fork and exec. Close-on-exec alone leaves a transient lease.
        unsafe {
            libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
impl SelectionLease {
    fn acquire(prefix: &Path, exclusive: bool) -> anyhow::Result<Self> {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(prefix.join("activation.lock"))?;
        let operation = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        } | libc::LOCK_NB;
        // SAFETY: valid descriptor; the descriptor owns the lock until drop.
        ensure!(
            unsafe { libc::flock(f.as_raw_fd(), operation) } == 0,
            "release activation is in progress; retry shortly"
        );
        Ok(Self { _file: f })
    }
}
pub fn start_lease() -> anyhow::Result<Option<SelectionLease>> {
    let Some(i) = installation::current()? else {
        return Ok(None);
    };
    if i.ownership.method != Method::Script {
        return Ok(None);
    }
    let lease = SelectionLease::acquire(&i.ownership.prefix, false)?;
    ensure!(
        i.ownership.prefix.join("current").canonicalize()? == i.root,
        "this executable is no longer the selected release; use {}/bin/lighter",
        i.ownership.prefix.display()
    );
    Ok(Some(lease))
}

fn link(target: &Path, path: &Path) -> anyhow::Result<()> {
    let parent = path.parent().context("link has no parent")?;
    fs::create_dir_all(parent)?;
    let tmp = tempfile::tempdir_in(parent)?;
    let staged = tmp.path().join("link");
    symlink(target, &staged)?;
    fs::rename(staged, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn select(prefix: &Path, root: &Path) -> anyhow::Result<()> {
    ensure!(
        root.parent() == Some(prefix.join("releases").as_path()),
        "release is outside managed generations"
    );
    link(
        &Path::new("releases").join(root.file_name().context("release has no name")?),
        &prefix.join("current"),
    )
}
fn public_paths(prefix: &Path) -> anyhow::Result<()> {
    link(
        Path::new("../current/bin/lighter"),
        &prefix.join("bin/lighter"),
    )?;
    let share = prefix.join("share/lighter");
    if let Ok(meta) = fs::symlink_metadata(&share)
        && !meta.file_type().is_symlink()
    {
        ensure!(meta.is_dir(), "unrecognized shared payload location");
        let backup = prefix.join("share/lighter-before-updater");
        ensure!(
            !backup.exists(),
            "legacy payload backup already exists; refusing to overwrite it"
        );
        fs::rename(&share, backup)?;
    }
    link(Path::new("../current/share/lighter"), &share)?;
    Ok(())
}
fn copy(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let out = Command::new("/bin/cp")
        .args(["-cR"])
        .arg(source)
        .arg(destination)
        .output()?;
    ensure!(
        out.status.success(),
        "could not stage release: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}
fn child(root: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new(root.join("bin/lighter"))
        .args(args)
        .env_remove("LIGHTER_GUEST_DIR")
        .status()?;
    ensure!(
        output.success(),
        "release command failed: {}",
        args.join(" ")
    );
    Ok(())
}
fn login_enabled(prefix: &Path) -> anyhow::Result<bool> {
    if !crate::paths::is_default_home() {
        return Ok(false);
    }
    let p = crate::service::plist_path()?;
    if !p.exists() {
        return Ok(false);
    }
    let exe =
        crate::service::configured_executable()?.context("login service has no executable")?;
    ensure!(
        exe.canonicalize()?.starts_with(prefix),
        "login service belongs to a different installation; resolve it before upgrading"
    );
    Ok(true)
}
fn stop_login() {
    let _ = crate::service::suspend();
}
fn restore(root: &Path, running: bool, login: bool) -> anyhow::Result<()> {
    if login {
        crate::service::refresh_at(root, running)?;
        if running {
            let deadline = std::time::Instant::now() + Duration::from_secs(120);
            while std::time::Instant::now() < deadline {
                if crate::machine::running_pid()?.is_some()
                    && crate::machine::docker_version(&crate::paths::docker_socket()?).is_ok()
                {
                    return verify_running(root);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            bail!("updated login service did not become ready within 120 seconds");
        }
    } else if running {
        child(root, &["start", "--timeout", "120"])?;
        verify_running(root)?;
    }
    Ok(())
}

fn verify_running(root: &Path) -> anyhow::Result<()> {
    let identity = crate::instance::Identity::read(&crate::paths::home()?)?
        .context("started VM has no authenticated identity")?;
    let expected = root
        .join("share/lighter/lighter.app/Contents/MacOS/lighter")
        .canonicalize()?;
    ensure!(
        identity.executable()?.canonicalize()? == expected,
        "a different runtime started during activation"
    );
    if let Ok(manifest) = release::read(root) {
        ensure!(
            identity.release_version.as_deref() == Some(manifest.version.as_str()),
            "running release differs from selected release"
        );
    }
    Ok(())
}

fn recover(prefix: &Path) -> anyhow::Result<()> {
    let path = prefix.join("upgrade.json");
    if !path.exists() {
        return Ok(());
    }
    let j: Journal = serde_json::from_slice(&fs::read(&path)?)?;
    let generations = prefix.join("releases");
    ensure!(
        j.next.parent() == Some(generations.as_path())
            && j.previous
                .as_ref()
                .is_none_or(|p| p.parent() == Some(generations.as_path())),
        "invalid recovery journal"
    );
    ensure!(
        crate::machine::running_pid()?.is_none(),
        "an interrupted upgrade needs recovery: stop lighter, then run upgrade again"
    );
    let lease = SelectionLease::acquire(prefix, true)?;
    let machine_lease = crate::instance::Instance::acquire(&crate::paths::home()?)?
        .context("VM is starting; retry recovery after it stops")?;
    if j.login {
        stop_login();
    }
    let root = j.previous.as_ref().unwrap_or(&j.next);
    select(prefix, root)?;
    public_paths(prefix)?;
    drop(machine_lease);
    drop(lease);
    restore(root, j.running, j.login)?;
    fs::remove_file(&path)?;
    bail!("recovered interrupted upgrade; review status and run the upgrade again when ready")
}

fn legacy(prefix: &Path) -> anyhow::Result<Option<PathBuf>> {
    let current = prefix.join("current");
    if current.exists() {
        let root = current.canonicalize()?;
        ensure!(
            root.parent() == Some(prefix.join("releases").as_path()),
            "current points outside managed releases"
        );
        return Ok(Some(root));
    }
    let bin = prefix.join("bin/lighter");
    if !bin.exists() {
        return Ok(None);
    }
    ensure!(
        !prefix.join("INSTALL_RECEIPT.json").exists(),
        "Homebrew installations must be upgraded with Brew"
    );
    // Adoption is explicit, but it must still not consume a source wrapper or
    // unrelated executable merely because it occupies the default directory.
    let requirement = format!(
        "=anchor apple generic and identifier \"lighter\" and certificate leaf[subject.OU] = \"{}\" and certificate leaf[field.1.2.840.113635.100.6.1.13] exists",
        release::TEAM
    );
    let output = Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", "-R", &requirement])
        .arg(&bin)
        .output()?;
    ensure!(
        output.status.success(),
        "existing executable is not an official release; refusing to replace it"
    );
    ensure!(
        prefix.join("share/lighter/Image").is_file()
            && prefix.join("share/lighter/rootfs.ext4").is_file(),
        "existing installation has no complete guest payload"
    );
    let root = prefix
        .join("releases")
        .join(format!("legacy-{}", &release::hash(&bin)?[..16]));
    if !root.exists() {
        let temp = tempfile::tempdir_in(prefix.join("releases"))?;
        fs::create_dir(temp.path().join("bin"))?;
        fs::create_dir(temp.path().join("share"))?;
        copy(&bin, &temp.path().join("bin/lighter"))?;
        copy(
            &prefix.join("share/lighter"),
            &temp.path().join("share/lighter"),
        )?;
        installation::write(temp.path(), prefix, Method::Script)?;
        fs::rename(temp.path(), &root)?;
    }
    Ok(Some(root))
}

pub fn install(source: &Path, prefix: &Path, restart: bool) -> anyhow::Result<()> {
    install_selected(source, prefix, restart, None)
}

fn unchanged_selection(prefix: &Path, expected: Option<&Path>) -> anyhow::Result<()> {
    if let Some(expected) = expected {
        ensure!(
            prefix.join("current").canonicalize()? == expected,
            "another installer changed the selected release; retry from the current executable"
        );
    }
    Ok(())
}

fn install_selected(
    source: &Path,
    prefix: &Path,
    restart: bool,
    expected: Option<&Path>,
) -> anyhow::Result<()> {
    let manifest = release::verify(source)?;
    fs::create_dir_all(prefix)?;
    let prefix = prefix.canonicalize()?;
    ensure!(
        !prefix
            .ancestors()
            .any(|p| p.file_name().is_some_and(|n| n == "Cellar")),
        "Brew manages the Cellar; refusing direct installation there"
    );
    let _operation = updates::Lock::acquire(&prefix.join(".installer"))?;
    recover(&prefix)?;
    let lease = SelectionLease::acquire(&prefix, true)?;
    // The download may have taken minutes. A separate installer can select a
    // newer generation meanwhile; validate under both activation locks so
    // this in-flight command cannot replace that newer selection.
    unchanged_selection(&prefix, expected)?;
    let running = crate::machine::running_pid()?.is_some();
    if running {
        // Do not stop a daily VM belonging to a different installation.
        let identity = crate::instance::Identity::read(&crate::paths::home()?)?.context("cannot establish running VM ownership; stop it manually before adopting this installation")?;
        ensure!(
            identity.executable()?.canonicalize()?.starts_with(&prefix),
            "running VM belongs to another installation; stop it explicitly first"
        );
        ensure!(
            restart,
            "release verified; the VM is running. Use `lighter upgrade --restart` (installer: --restart) to activate it"
        );
    }
    let mut machine_lease = if running {
        None
    } else {
        Some(
            crate::instance::Instance::acquire(&crate::paths::home()?)?
                .context("VM is starting; retry after it is ready or stopped")?,
        )
    };
    let login = login_enabled(&prefix)?;
    fs::create_dir_all(prefix.join("releases"))?;
    let root = prefix.join("releases").join(&manifest.version);
    if root.exists() {
        ensure!(
            {
                let existing = release::verify(&root)?;
                existing.version == manifest.version && existing.files == manifest.files
            },
            "a different payload already occupies this version"
        );
    } else {
        let temp = tempfile::tempdir_in(prefix.join("releases"))?;
        let staged = temp.path().join("release");
        copy(source, &staged)?;
        release::verify(&staged)?;
        installation::write(&staged, &prefix, Method::Script)?;
        fs::rename(staged, &root)?;
    }
    installation::write(&root, &prefix, Method::Script)?;
    let previous = legacy(&prefix)?;
    if previous.as_ref() == Some(&root) {
        println!("Already installed: {}", manifest.version);
        return Ok(());
    }
    let journal = Journal {
        previous,
        next: root.clone(),
        running,
        login,
    };
    installation::atomic_json(&prefix.join("upgrade.json"), &journal)?;
    let switched = (|| {
        if login {
            crate::service::suspend()?;
        }
        if running {
            crate::machine::stop(Duration::from_secs(30))?;
        }
        ensure!(
            crate::machine::running_pid()?.is_none(),
            "machine did not stop"
        );
        if machine_lease.is_none() {
            machine_lease = Some(
                crate::instance::Instance::acquire(&crate::paths::home()?)?
                    .context("VM did not release its lifetime lock")?,
            );
        }
        select(&prefix, &root)?;
        public_paths(&prefix)?;
        Ok::<_, anyhow::Error>(())
    })();
    drop(machine_lease);
    drop(lease);
    let activated = switched.and_then(|_| restore(&root, running, login));
    if let Err(error) = activated {
        if let Some(previous) = &journal.previous {
            if journal.login {
                crate::service::suspend()?;
            }
            if journal.running {
                crate::machine::stop(Duration::from_secs(30))?;
            }
            let rollback_lease = SelectionLease::acquire(&prefix, true)?;
            let machine_lease = crate::instance::Instance::acquire(&crate::paths::home()?)?
                .context("VM is still starting; recovery journal retained")?;
            select(&prefix, previous)?;
            public_paths(&prefix)?;
            drop(machine_lease);
            drop(rollback_lease);
            restore(previous, running, login)?;
            fs::remove_file(prefix.join("upgrade.json"))?;
            bail!("upgrade failed; restored previous release: {error:#}");
        }
        return Err(error.context("activation failed; rerun the installer to recover"));
    }
    fs::remove_file(prefix.join("upgrade.json"))?;
    println!(
        "Installed lighter {} (Linux {}).",
        manifest.version, manifest.kernel_version
    );
    Ok(())
}

pub fn run(restart: bool) -> anyhow::Result<()> {
    let i = updates::owned()?;
    updates::direct(&i)?;
    let _lock = updates::Lock::acquire(&updates::state_dir(&i)?)?;
    {
        let _recovery = updates::Lock::acquire(&i.ownership.prefix.join(".installer"))?;
        recover(&i.ownership.prefix)?;
    }
    let root = if let Some(root) = updates::staged(&i)? {
        Some(root)
    } else {
        updates::fetch(&i, true)?
    };
    if let Some(root) = root {
        install_selected(&root, &i.ownership.prefix, restart, Some(&i.root))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selector_never_mixes_release_paths() {
        let t = tempfile::tempdir().unwrap();
        let prefix = t.path().canonicalize().unwrap();
        for version in ["0.4.2", "0.4.3"] {
            let root = prefix.join("releases").join(version);
            fs::create_dir_all(root.join("bin")).unwrap();
            fs::create_dir_all(root.join("share/lighter")).unwrap();
            fs::write(root.join("bin/lighter"), version).unwrap();
            fs::write(root.join("share/lighter/Image"), version).unwrap();
            select(&prefix, &root).unwrap();
            public_paths(&prefix).unwrap();
            assert_eq!(
                fs::read_to_string(prefix.join("bin/lighter")).unwrap(),
                version
            );
            assert_eq!(
                fs::read_to_string(prefix.join("share/lighter/Image")).unwrap(),
                version
            );
        }
    }
    #[test]
    fn an_in_flight_update_cannot_replace_a_newer_selection() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().canonicalize().unwrap();
        let original = prefix.join("releases/0.4.2");
        let newer = prefix.join("releases/0.4.4");
        fs::create_dir_all(&original).unwrap();
        fs::create_dir_all(&newer).unwrap();
        select(&prefix, &original).unwrap();
        let observed = prefix.join("current").canonicalize().unwrap();
        // An installer wins while the original updater is downloading 0.4.3.
        select(&prefix, &newer).unwrap();
        let _lease = SelectionLease::acquire(&prefix, true).unwrap();
        assert!(unchanged_selection(&prefix, Some(&observed)).is_err());
        assert_eq!(prefix.join("current").canonicalize().unwrap(), newer);
        assert!(unchanged_selection(&prefix, Some(&newer)).is_ok());
    }
    #[test]
    fn activation_excludes_concurrent_start() {
        let t = tempfile::tempdir().unwrap();
        let guard = SelectionLease::acquire(t.path(), true).unwrap();
        assert!(SelectionLease::acquire(t.path(), false).is_err());
        drop(guard);
        assert!(SelectionLease::acquire(t.path(), false).is_ok());
    }
}
