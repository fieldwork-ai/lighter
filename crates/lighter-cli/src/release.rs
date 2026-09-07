//! A release is one authenticated set of host and guest artifacts.
use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    process::Command,
};

pub const MANIFEST: &str = "share/lighter/lighter.app/Contents/Resources/release.json";
pub const TEAM: &str = "N7N6BNF95K";
const REQUIRED: [&str; 4] = [
    "bin/lighter",
    "share/lighter/Image",
    "share/lighter/rootfs.ext4",
    "share/lighter/kernel.version",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub version: String,
    pub kernel_version: String,
    pub data_epoch: u32,
    pub files: BTreeMap<String, String>,
}

pub fn hash(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 128 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn stable_version(text: &str) -> anyhow::Result<semver::Version> {
    let version = semver::Version::parse(text)?;
    ensure!(
        version.pre.is_empty() && version.build.is_empty(),
        "only stable releases are supported"
    );
    Ok(version)
}

pub fn read(root: &Path) -> anyhow::Result<Manifest> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(root.join(MANIFEST))?)?;
    ensure!(
        manifest.schema == 1 && manifest.data_epoch == 1,
        "unsupported release format"
    );
    stable_version(&manifest.version)?;
    ensure!(
        !manifest.kernel_version.is_empty(),
        "missing kernel version"
    );
    ensure!(
        manifest.files.len() == REQUIRED.len(),
        "unexpected release artifact list"
    );
    for name in REQUIRED {
        let value = manifest
            .files
            .get(name)
            .context("missing release artifact")?;
        ensure!(
            value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid artifact digest"
        );
    }
    Ok(manifest)
}

fn checked(command: &mut Command) -> anyhow::Result<()> {
    let out = command.output()?;
    ensure!(
        out.status.success(),
        "release verification failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

pub fn verify(root: &Path) -> anyhow::Result<Manifest> {
    let app = root.join("share/lighter/lighter.app");
    let requirement = format!(
        "anchor apple generic and identifier \"dev.lighter.machine\" and certificate leaf[subject.OU] = \"{TEAM}\" and certificate leaf[field.1.2.840.113635.100.6.1.13] exists"
    );
    checked(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict", "--deep", "-R", &requirement])
            .arg(&app),
    )?;
    checked(
        Command::new("/usr/sbin/spctl")
            .args(["--assess", "--type", "execute"])
            .arg(&app),
    )?;
    let manifest = read(root)?;
    verify_payload(root, &manifest)?;
    Ok(manifest)
}

fn verify_payload(root: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    for (name, expected) in &manifest.files {
        let path = root.join(name);
        ensure!(
            fs::symlink_metadata(&path)?.file_type().is_file(),
            "artifact is not a regular file: {name}"
        );
        ensure!(
            &hash(&path)? == expected,
            "artifact checksum mismatch: {name}"
        );
    }
    ensure!(
        fs::read_to_string(root.join("share/lighter/kernel.version"))?.trim()
            == manifest.kernel_version,
        "kernel version does not match the signed manifest"
    );
    Ok(())
}

/// Extract only directories and ordinary/sparse files, never links or devices.
/// The destination must be an empty private directory owned by this operation.
pub fn extract(archive: &Path, destination: &Path) -> anyhow::Result<PathBuf> {
    ensure!(
        fs::read_dir(destination)?.next().is_none(),
        "staging directory is not empty"
    );
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?));
    let mut names = BTreeSet::new();
    let mut root: Option<PathBuf> = None;
    let mut bytes = 0u64;
    for entry in ar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        ensure!(
            !path.as_os_str().is_empty()
                && path.components().all(|c| matches!(c, Component::Normal(_))),
            "unsafe archive path: {}",
            path.display()
        );
        ensure!(names.insert(path.clone()), "duplicate archive entry");
        let first = PathBuf::from(path.components().next().unwrap().as_os_str());
        if let Some(root) = &root {
            ensure!(*root == first, "multiple archive roots");
        } else {
            root = Some(first);
        }
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir() || kind.is_gnu_sparse(),
            "archive links and special files are forbidden"
        );
        bytes = bytes
            .checked_add(entry.size())
            .context("archive size overflow")?;
        ensure!(
            bytes <= 16 * 1024 * 1024 * 1024 && names.len() <= 4096,
            "release archive exceeds extraction limits"
        );
        entry.set_mask(0o022);
        ensure!(
            entry.unpack_in(destination)?,
            "archive escaped staging directory"
        );
    }
    match root {
        Some(root) => Ok(destination.join(root)),
        None => bail!("empty release archive"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_stable_versions() {
        assert!(stable_version("0.4.2").unwrap() > stable_version("0.4.1").unwrap());
        for v in ["../0.4.2", "0.4.2-rc.1", "0.4.2+other", "latest"] {
            assert!(stable_version(v).is_err());
        }
    }
    #[test]
    fn rejects_links_before_extraction() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bad.tgz");
        let encoder = flate2::write::GzEncoder::new(
            File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut ar = tar::Builder::new(encoder);
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        h.set_cksum();
        ar.append_link(&mut h, "release/escape", "/tmp").unwrap();
        ar.into_inner().unwrap().finish().unwrap();
        let stage = tempfile::tempdir().unwrap();
        assert!(extract(&path, stage.path()).is_err());
        assert!(!stage.path().join("release/escape").exists());
    }
    #[test]
    fn payload_tampering_and_missing_artifacts_fail_closed() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let mut files = BTreeMap::new();
        for name in REQUIRED {
            let p = root.join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(
                &p,
                if name.ends_with("kernel.version") {
                    "6.18.49\n"
                } else {
                    "original"
                },
            )
            .unwrap();
            files.insert(name.to_owned(), hash(&p).unwrap());
        }
        let m = Manifest {
            schema: 1,
            version: "0.4.2".into(),
            kernel_version: "6.18.49".into(),
            data_epoch: 1,
            files,
        };
        assert!(verify_payload(root, &m).is_ok());
        fs::write(root.join("share/lighter/Image"), "changed").unwrap();
        assert!(verify_payload(root, &m).is_err());
        fs::write(root.join("share/lighter/Image"), "original").unwrap();
        fs::remove_file(root.join("share/lighter/rootfs.ext4")).unwrap();
        assert!(verify_payload(root, &m).is_err());
    }

    #[test]
    fn extraction_rejects_traversal_absolute_paths_and_duplicate_files() {
        for name in ["../escape", "/absolute", "root/../../escape"] {
            let temp = tempfile::tempdir().unwrap();
            let archive = temp.path().join("bad.tgz");
            let encoder = flate2::write::GzEncoder::new(
                File::create(&archive).unwrap(),
                flate2::Compression::fast(),
            );
            let mut ar = tar::Builder::new(encoder);
            let mut h = tar::Header::new_gnu();
            h.set_size(1);
            h.set_mode(0o644);
            h.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            h.set_cksum();
            ar.append(&h, &b"x"[..]).unwrap();
            ar.into_inner().unwrap().finish().unwrap();
            let stage = tempfile::tempdir().unwrap();
            assert!(extract(&archive, stage.path()).is_err(), "{name}");
        }
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("duplicate.tgz");
        let encoder = flate2::write::GzEncoder::new(
            File::create(&archive).unwrap(),
            flate2::Compression::fast(),
        );
        let mut ar = tar::Builder::new(encoder);
        for _ in 0..2 {
            let mut h = tar::Header::new_gnu();
            h.set_size(1);
            h.set_mode(0o644);
            h.set_cksum();
            ar.append_data(&mut h, "release/file", &b"x"[..]).unwrap();
        }
        ar.into_inner().unwrap().finish().unwrap();
        let stage = tempfile::tempdir().unwrap();
        assert!(extract(&archive, stage.path()).is_err());
    }
}
