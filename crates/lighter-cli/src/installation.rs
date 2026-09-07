//! Installer ownership is attached to an installation, not a VM home.
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

const MARKER: &str = "share/lighter/installation.json";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    Script,
    Homebrew,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installation {
    pub schema: u32,
    pub method: Method,
    pub id: String,
    pub prefix: PathBuf,
}

pub fn identifier(prefix: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(prefix.as_os_str().as_encoded_bytes())
    )[..24]
        .to_owned()
}

pub fn payload(executable: &Path) -> Option<PathBuf> {
    let exe = executable.canonicalize().ok()?;
    if exe.parent()?.file_name()? == "bin" {
        return exe.parent()?.parent().map(Path::to_owned);
    }
    // release/share/lighter/lighter.app/Contents/MacOS/lighter
    let root = exe.ancestors().nth(6)?;
    (root.join("share/lighter/lighter.app/Contents/MacOS/lighter") == exe).then(|| root.to_owned())
}

pub fn detect(executable: &Path) -> anyhow::Result<Option<SelfInstallation>> {
    let Some(root) = payload(executable) else {
        return Ok(None);
    };
    if !root.join(MARKER).exists() {
        if root.join("INSTALL_RECEIPT.json").is_file() {
            let prefix = root.parent().context("missing Brew package")?.to_owned();
            ensure!(
                prefix.file_name().is_some_and(|n| n == "lighter")
                    && prefix
                        .parent()
                        .is_some_and(|p| p.file_name().is_some_and(|n| n == "Cellar")),
                "unrecognized Homebrew receipt location"
            );
            return Ok(Some(SelfInstallation {
                root,
                ownership: Installation {
                    schema: 1,
                    method: Method::Homebrew,
                    id: identifier(&prefix),
                    prefix,
                },
            }));
        }
        return Ok(None);
    }
    let ownership: Installation = serde_json::from_slice(&fs::read(root.join(MARKER))?)?;
    ensure!(
        ownership.schema == 1
            && ownership.prefix.is_absolute()
            && ownership.prefix.canonicalize()? == ownership.prefix
            && ownership.id == identifier(&ownership.prefix),
        "installation metadata does not match its prefix"
    );
    match ownership.method {
        Method::Script => ensure!(
            root.parent() == Some(ownership.prefix.join("releases").as_path()),
            "installation was moved; reinstall to adopt this location"
        ),
        Method::Homebrew => ensure!(
            root.join("INSTALL_RECEIPT.json").is_file()
                && root.parent() == Some(ownership.prefix.as_path())
                && ownership.prefix.file_name().is_some_and(|n| n == "lighter")
                && ownership
                    .prefix
                    .parent()
                    .is_some_and(|p| p.file_name().is_some_and(|n| n == "Cellar")),
            "Homebrew ownership does not match its receipt"
        ),
    }
    Ok(Some(SelfInstallation { root, ownership }))
}

#[derive(Debug)]
pub struct SelfInstallation {
    pub root: PathBuf,
    pub ownership: Installation,
}

pub fn current() -> anyhow::Result<Option<SelfInstallation>> {
    detect(&std::env::current_exe()?)
}

pub fn write(root: &Path, prefix: &Path, method: Method) -> anyhow::Result<Installation> {
    let prefix = prefix.canonicalize()?;
    let record = Installation {
        schema: 1,
        id: identifier(&prefix),
        method,
        prefix,
    };
    atomic_json(&root.join(MARKER), &record)?;
    Ok(record)
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = path.parent().context("path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(&serde_json::to_vec_pretty(value)?)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn describe() -> String {
    let exe = std::env::current_exe().unwrap_or_default();
    let resolved = exe.canonicalize().unwrap_or(exe);
    let method = match current() {
        Ok(Some(i)) => match i.ownership.method {
            Method::Script => "install script".to_owned(),
            Method::Homebrew => "Homebrew".to_owned(),
        },
        Ok(None)
            if resolved.ancestors().any(|p| {
                p.join("Cargo.toml").is_file() && p.join("crates/lighter-cli").is_dir()
            }) =>
        {
            "source build".to_owned()
        }
        Ok(None) => "manual / legacy (unmarked)".to_owned(),
        Err(e) => format!("conflicting metadata: {e}"),
    };
    format!("{method}; {}", resolved.display())
}

pub fn version_report() -> String {
    let selected = current().ok().flatten().and_then(|i| {
        let root = if i.ownership.method == Method::Script {
            i.ownership.prefix.join("current")
        } else {
            i.root
        };
        crate::release::read(&root).ok()
    });
    let installed = selected
        .as_ref()
        .map(|m| m.version.as_str())
        .unwrap_or(env!("CARGO_PKG_VERSION"));
    let mut text = format!(
        "  installation  {}\n  CLI version   {}\n  installed     {}\n  kernel image  {}\n",
        describe(),
        env!("CARGO_PKG_VERSION"),
        installed,
        selected
            .as_ref()
            .map(|m| m.kernel_version.as_str())
            .unwrap_or("unknown")
    );
    if let Ok(home) = crate::paths::home()
        && let Ok(Some(identity)) = crate::instance::Identity::read(&home)
    {
        text.push_str(&format!(
            "  running       {}\n  kernel        {}\n",
            identity
                .release_version
                .as_deref()
                .unwrap_or("unknown (older daemon)"),
            identity.kernel_version.as_deref().unwrap_or("unknown")
        ));
    } else {
        text.push_str("  running       stopped or unknown\n");
    }
    if let Ok(Some(i)) = current()
        && let Ok(s) = crate::updates::state(&i)
        && let Some(version) = s.downloaded
        && let (Ok(new), Ok(old)) = (
            crate::release::stable_version(&version),
            crate::release::stable_version(installed),
        )
        && new > old
    {
        text.push_str(&format!("  downloaded    {version}\n"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copied_marker_cannot_claim_another_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("owned");
        let root = prefix.join("releases/0.4.2");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/lighter"), "test").unwrap();
        write(&root, &prefix, Method::Script).unwrap();
        assert!(detect(&root.join("bin/lighter")).unwrap().is_some());
        let moved = temp.path().join("moved");
        fs::rename(&prefix, &moved).unwrap();
        assert!(detect(&moved.join("releases/0.4.2/bin/lighter")).is_err());
    }
    #[test]
    fn brew_receipt_and_marker_must_agree() {
        let t = tempfile::tempdir().unwrap();
        let prefix = t.path().join("Cellar/lighter");
        let root = prefix.join("0.4.2");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/lighter"), "test").unwrap();
        fs::write(root.join("INSTALL_RECEIPT.json"), "{}").unwrap();
        let found = detect(&root.join("bin/lighter")).unwrap().unwrap();
        assert_eq!(found.ownership.method, Method::Homebrew);
        write(&root, &prefix, Method::Homebrew).unwrap();
        assert!(detect(&root.join("bin/lighter")).unwrap().is_some());
        fs::remove_file(root.join("INSTALL_RECEIPT.json")).unwrap();
        assert!(detect(&root.join("bin/lighter")).is_err());
    }
    #[test]
    fn legacy_is_not_silently_adopted() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("bin")).unwrap();
        fs::write(t.path().join("bin/lighter"), "x").unwrap();
        assert!(detect(&t.path().join("bin/lighter")).unwrap().is_none());
    }
}
