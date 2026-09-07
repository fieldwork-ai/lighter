//! Downloads are independent of activation and never run in the VMM.
use crate::{
    installation::{self, Method, SelfInstallation},
    release,
};
use anyhow::{Context, bail, ensure};
use clap::{Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Subcommand)]
pub enum Action {
    /// Check GitHub for a newer stable release.
    Check,
    /// Download and verify an update without activating it.
    Download,
    /// Enable or disable daily background downloads (direct installs only).
    AutoDownload {
        #[arg(value_enum)]
        mode: Toggle,
    },
    #[command(hide = true)]
    Poll,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum Toggle {
    On,
    Off,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub automatic: bool,
    pub attempted: u64,
    pub checked: u64,
    pub available: Option<String>,
    pub downloaded: Option<String>,
    pub error: Option<String>,
}
#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub struct Lock {
    _file: File,
}
impl Drop for Lock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // See Instance: children may inherit this descriptor before exec.
        unsafe {
            libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
impl Lock {
    pub fn acquire(prefix: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(prefix)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(prefix.join("operation.lock"))?;
        ensure!(
            crate::instance::try_lock(&file)?,
            "another updater is working on this installation"
        );
        Ok(Self { _file: file })
    }
}

pub fn state_dir(i: &SelfInstallation) -> anyhow::Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").context("HOME is unset")?)
            .join("Library/Application Support/lighter/updates")
            .join(&i.ownership.id),
    )
}
pub fn state(i: &SelfInstallation) -> anyhow::Result<State> {
    let path = state_dir(i)?.join("state.json");
    if !path.exists() {
        return Ok(State::default());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn save(i: &SelfInstallation, value: &State) -> anyhow::Result<()> {
    installation::atomic_json(&state_dir(i)?.join("state.json"), value)
}
fn due(s: &State, time: u64) -> bool {
    s.automatic
        && time.saturating_sub(s.checked) >= 86400
        && time.saturating_sub(s.attempted) >= 3600
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn owned() -> anyhow::Result<SelfInstallation> {
    installation::current()?.context("this installation is not managed; rerun the official installer for a direct installation, or update your source/manual build")
}
pub fn direct(i: &SelfInstallation) -> anyhow::Result<()> {
    ensure!(
        i.ownership.method == Method::Script,
        "Homebrew manages this installation. Run: brew upgrade fieldwork-ai/tap/lighter; then restart lighter when ready"
    );
    ensure!(
        i.ownership.prefix.join("current").canonicalize()? == i.root,
        "use the selected executable: {}/bin/lighter",
        i.ownership.prefix.display()
    );
    ensure!(
        crate::paths::is_default_home() && std::env::var_os("LIGHTER_GUEST_DIR").is_none(),
        "updates must be run without LIGHTER_HOME or LIGHTER_GUEST_DIR overrides"
    );
    Ok(())
}

fn curl(url: &str) -> Command {
    let mut c = Command::new("/usr/bin/curl");
    c.args([
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "15",
        "--max-time",
        "900",
        "--max-filesize",
        "2147483648",
        "--user-agent",
        concat!("lighter/", env!("CARGO_PKG_VERSION")),
        url,
    ]);
    c
}
fn candidate(bytes: &[u8]) -> anyhow::Result<(String, String)> {
    let r: GithubRelease = serde_json::from_slice(bytes)?;
    ensure!(!r.draft && !r.prerelease, "not a stable release");
    let version = r
        .tag_name
        .strip_prefix('v')
        .context("release tag has no v prefix")?;
    release::stable_version(version)?;
    let name = format!("lighter-{version}-arm64.tar.gz");
    let expected =
        format!("https://github.com/fieldwork-ai/lighter/releases/download/v{version}/{name}");
    let asset = r
        .assets
        .iter()
        .find(|a| a.name == name)
        .context("release has no Apple Silicon archive")?;
    ensure!(
        asset.browser_download_url == expected,
        "unexpected release download origin"
    );
    Ok((version.to_owned(), expected))
}

pub fn fetch(i: &SelfInstallation, download: bool) -> anyhow::Result<Option<PathBuf>> {
    let mut s = state(i)?;
    s.attempted = now();
    save(i, &s)?;
    let result = (|| {
        let out = curl("https://api.github.com/repos/fieldwork-ai/lighter/releases/latest")
            .args([
                "--header",
                "Accept: application/vnd.github+json",
                "--max-time",
                "30",
                "--max-filesize",
                "1048576",
            ])
            .output()?;
        ensure!(
            out.status.success(),
            "release check failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        let (version, url) = candidate(&out.stdout)?;
        s.checked = now();
        s.available = Some(version.clone());
        s.error = None;
        let installed = release::read(&i.root)
            .map(|m| m.version)
            .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").into());
        if release::stable_version(&version)? <= release::stable_version(&installed)? {
            println!("Already up to date ({installed}).");
            return Ok(None);
        }
        if !download {
            println!("Lighter {version} is available (installed {installed}).");
            return Ok(None);
        }
        direct(i)?;
        let cache = state_dir(i)?;
        fs::create_dir_all(&cache)?;
        let final_dir = cache.join(format!("release-{version}"));
        if final_dir.exists() {
            ensure!(
                release::verify(&final_dir)?.version == version,
                "cached release version mismatch"
            );
        } else {
            for entry in fs::read_dir(&cache)? {
                let entry = entry?;
                if entry.file_name().to_string_lossy().starts_with("download-")
                    && entry.file_type()?.is_dir()
                {
                    fs::remove_dir_all(entry.path())?;
                }
            }
            let temp = tempfile::Builder::new()
                .prefix("download-")
                .tempdir_in(&cache)?;
            let archive = temp.path().join("release.tgz");
            let output = curl(&url).arg("--output").arg(&archive).output()?;
            ensure!(
                output.status.success(),
                "download failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            let unpack = temp.path().join("unpack");
            fs::create_dir(&unpack)?;
            let root = release::extract(&archive, &unpack)?;
            ensure!(
                release::verify(&root)?.version == version,
                "downloaded version differs from requested release"
            );
            fs::rename(root, &final_dir)?;
        }
        s.downloaded = Some(version.clone());
        println!("Lighter {version} is downloaded and verified. Activate with `lighter upgrade`.");
        Ok(Some(final_dir))
    })();
    if let Err(e) = &result {
        s.error = Some(format!("{e:#}"));
    }
    save(i, &s)?;
    result
}

pub fn staged(i: &SelfInstallation) -> anyhow::Result<Option<PathBuf>> {
    let s = state(i)?;
    if let Some(version) = s.downloaded
        && release::stable_version(&version)? > release::stable_version(env!("CARGO_PKG_VERSION"))?
    {
        let root = state_dir(i)?.join(format!("release-{version}"));
        ensure!(
            release::verify(&root)?.version == version,
            "cached version mismatch"
        );
        return Ok(Some(root));
    }
    Ok(None)
}

pub fn run(action: Action) -> anyhow::Result<()> {
    let i = owned()?;
    if !matches!(action, Action::Check) {
        direct(&i)?;
    }
    let _lock = Lock::acquire(&state_dir(&i)?)?;
    match action {
        Action::Check => {
            fetch(&i, false)?;
        }
        Action::Download => {
            fetch(&i, true)?;
        }
        Action::AutoDownload { mode } => {
            let enabled = matches!(mode, Toggle::On);
            let mut s = state(&i)?;
            // Save before enabling so an immediately launched poll sees opt-in.
            let previous = s.automatic;
            s.automatic = enabled;
            save(&i, &s)?;
            if let Err(error) = configure_agent(&i, enabled) {
                s.automatic = previous;
                save(&i, &s)?;
                return Err(error);
            }
            println!(
                "Automatic downloads {}. Activation always requires `lighter upgrade`.",
                if enabled { "enabled" } else { "disabled" }
            );
        }
        Action::Poll => {
            let s = state(&i)?;
            let time = now();
            if due(&s, time) {
                fetch(&i, true)?;
            }
        }
    }
    Ok(())
}

pub fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn configure_agent(i: &SelfInstallation, enabled: bool) -> anyhow::Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is unset")?);
    let label = format!("dev.lighter.updates.{}", i.ownership.id);
    let path = home
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"));
    let target = format!("gui/{}", unsafe { libc::getuid() });
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("{target}/{label}")])
        .output();
    if !enabled {
        if path.exists() {
            fs::remove_file(path)?;
        }
        return Ok(());
    }
    fs::create_dir_all(path.parent().unwrap())?;
    let exe = xml(&i.ownership.prefix.join("bin/lighter").to_string_lossy());
    let log = xml(&state_dir(i)?.join("updates.log").to_string_lossy());
    fs::write(
        &path,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{exe}</string><string>update</string><string>poll</string></array><key>StartInterval</key><integer>3600</integer><key>RunAtLoad</key><true/><key>ProcessType</key><string>Background</string><key>LowPriorityIO</key><true/><key>StandardOutPath</key><string>{log}</string><key>StandardErrorPath</key><string>{log}</string></dict></plist>"#
        ),
    )?;
    let output = Command::new("/bin/launchctl")
        .arg("bootstrap")
        .arg(target)
        .arg(path)
        .output()?;
    if !output.status.success() {
        bail!(
            "could not register background downloader: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

pub fn notice() {
    use std::io::IsTerminal;
    if !std::io::stderr().is_terminal() {
        return;
    }
    if let Ok(Some(i)) = installation::current()
        && let Ok(s) = state(&i)
        && let Some(version) = s.available
        && let (Ok(new), Ok(current)) = (
            release::stable_version(&version),
            release::stable_version(env!("CARGO_PKG_VERSION")),
        )
        && new > current
    {
        let command = if i.ownership.method == Method::Homebrew {
            "brew upgrade fieldwork-ai/tap/lighter"
        } else {
            "lighter upgrade"
        };
        eprintln!("Update available: lighter {version}. Run `{command}` when ready.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn background_downloads_require_opt_in_and_back_off() {
        let mut s = State::default();
        assert!(!due(&s, 100000));
        s.automatic = true;
        assert!(due(&s, 100000));
        s.attempted = 100000;
        assert!(!due(&s, 100001));
        assert!(due(&s, 103600));
        s.checked = 100000;
        assert!(!due(&s, 103600));
        assert!(due(&s, 186400));
        assert!(!due(&s, 1));
    }
    #[test]
    fn release_origin_and_channel_are_checked() {
        let mut r = serde_json::json!({"tag_name":"v0.4.3", "draft":false, "prerelease":false, "assets":[{"name":"lighter-0.4.3-arm64.tar.gz", "browser_download_url":"https://github.com/fieldwork-ai/lighter/releases/download/v0.4.3/lighter-0.4.3-arm64.tar.gz"}]});
        assert!(candidate(&serde_json::to_vec(&r).unwrap()).is_ok());
        r["draft"] = true.into();
        assert!(candidate(&serde_json::to_vec(&r).unwrap()).is_err());
        r["draft"] = false.into();
        r["assets"][0]["browser_download_url"] = "https://other.invalid/payload".into();
        assert!(candidate(&serde_json::to_vec(&r).unwrap()).is_err());
    }
}
