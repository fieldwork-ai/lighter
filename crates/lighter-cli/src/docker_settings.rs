//! The user's own Docker settings: `~/.lighter/docker/daemon.json`, in
//! Docker's format, read by the guest's dockerd (`--config-file`).
//!
//! The guest's root filesystem is lighter's and is replaced by every
//! upgrade, so a `daemon.json` written there lasted until the next one
//! (#111). This one lives in lighter's home on the Mac, which upgrades do not
//! touch. The machine shares its folder with the guest at start, read-only,
//! and init hands it to dockerd; a change takes effect at the next start.
//!
//! What dockerd accepts is dockerd's to say, not a list kept here: lighter
//! sets some of its options itself (its sockets, storage, MTU, IPv6, and the
//! address pools while direct access is on), and dockerd refuses a file that
//! sets one of those too, as it refuses an unknown key or a bad value. A
//! refused file does not cost the machine its Docker: init starts dockerd
//! again without it, and `lighter start` and `lighter doctor` give dockerd's
//! reason. Only JSON that does not parse is caught here, before the machine
//! boots, where the line and column can still be named.

use std::path::{Path, PathBuf};

use crate::doctor::Finding;

/// The folder the guest is given: the file and nothing else of the home.
pub fn dir(home: &Path) -> PathBuf {
    home.join("docker")
}

/// The settings file.
pub fn path(home: &Path) -> PathBuf {
    dir(home).join("daemon.json")
}

/// The share's tag, which init mounts at a fixed place when told.
pub const TAG: &str = "dockerconf";

/// Where the guest's init mounts the folder, and so the path dockerd's
/// messages name.
const GUEST_PATH: &str = "/etc/lighter/docker/daemon.json";

/// The file as the user would write it in a message: `~/.lighter/...` for
/// the default home.
pub fn shown(home: &Path) -> String {
    if crate::paths::is_default_home() {
        "~/.lighter/docker/daemon.json".into()
    } else {
        path(home).display().to_string()
    }
}

/// The file, as found on the Mac.
#[derive(Debug, PartialEq, Eq)]
pub enum Local {
    Absent,
    Valid,
    /// Not a JSON object: serde's words, with the line and column.
    Invalid(String),
}

pub fn local(home: &Path) -> Local {
    match std::fs::read_to_string(path(home)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Local::Absent,
        Err(e) => Local::Invalid(e.to_string()),
        Ok(text) => judge(&text),
    }
}

fn judge(text: &str) -> Local {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Object(_)) => Local::Valid,
        Ok(_) => Local::Invalid("it is not a JSON object ({ ... })".into()),
        Err(e) => Local::Invalid(e.to_string()),
    }
}

/// What the file was when the machine started, kept in the home so `doctor`
/// can say when it has changed since: its size and modification time.
pub fn stamp(home: &Path) -> Option<String> {
    let meta = std::fs::metadata(path(home)).ok()?;
    Some(format!("{:?} {}", meta.modified().ok()?, meta.len()))
}

/// Where the machine records the stamp of the file it started with.
pub fn stamp_file(home: &Path) -> PathBuf {
    home.join("docker-settings.from")
}

/// What the guest made of the file, from its agent (`docker-config`).
#[derive(Debug, PartialEq, Eq)]
pub enum InGuest {
    /// Not given one: none on the Mac, or not valid JSON there.
    None,
    Applied,
    /// dockerd would not start with it, and runs without it; its reason.
    Refused(String),
    /// The folder could not be mounted.
    Unmounted,
}

impl InGuest {
    pub fn parse(reply: &str) -> Option<InGuest> {
        let rest = reply.trim().strip_prefix("docker-config")?.trim();
        Some(
            match rest
                .split_once(' ')
                .map_or((rest, ""), |(w, r)| (w, r.trim()))
            {
                ("none", _) => InGuest::None,
                ("applied", _) => InGuest::Applied,
                ("unmounted", _) => InGuest::Unmounted,
                ("refused", why) => InGuest::Refused(why.to_string()),
                _ => return None,
            },
        )
    }
}

/// The finding for `doctor`, and for `lighter start` once Docker answers:
/// nothing when there is no file and the guest was given none.
pub fn finding(home: &Path, applied: Option<InGuest>) -> Option<Finding> {
    let file = shown(home);
    let local = local(home);
    if let Local::Invalid(why) = &local {
        return Some(Finding::warn(
            "docker settings",
            format!("{file} is not valid JSON, so Docker runs without it: {why}"),
            "fix it, then `lighter restart`",
        ));
    }
    if local == Local::Absent && matches!(applied, None | Some(InGuest::None)) {
        return None;
    }
    let changed = local == Local::Valid
        && std::fs::read_to_string(stamp_file(home)).ok().as_deref() != stamp(home).as_deref();
    Some(match applied {
        Some(InGuest::Refused(why)) => Finding::warn(
            "docker settings",
            format!(
                "Docker would not start with {file}, so it runs without it: {}",
                // dockerd names the file where the guest mounts it.
                why.replace(GUEST_PATH, &file)
            ),
            "fix it, then `lighter restart`. Settings lighter makes itself (its sockets, storage, MTU, IPv6, and the address pools while direct access is on) cannot be set there too",
        ),
        Some(InGuest::Unmounted) => Finding::warn(
            "docker settings",
            format!("the machine could not read {file}, so Docker runs without it"),
            "`lighter restart`; `lighter logs` has the machine's side",
        ),
        _ if local == Local::Absent => Finding::warn(
            "docker settings",
            format!("{file} is gone, but Docker started with it"),
            "`lighter restart` starts Docker without it",
        ),
        _ if changed => Finding::warn(
            "docker settings",
            format!("{file} has changed since the machine started"),
            "`lighter restart` applies it",
        ),
        Some(InGuest::Applied) => Finding::good("docker settings", format!("from {file}")),
        // A machine before 0.13.11, or one that did not say.
        _ => Finding::warn(
            "docker settings",
            format!("{file} is there, but the machine does not say whether Docker uses it"),
            "`lighter restart`",
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_json_object_is_valid() {
        assert_eq!(
            judge(r#"{"default-address-pools":[{"base":"10.20.0.0/16","size":24}]}"#),
            Local::Valid
        );
        assert_eq!(judge("{}"), Local::Valid);
        assert!(matches!(judge("[]"), Local::Invalid(_)));
        match judge("{\n  \"debug\": true,\n}") {
            Local::Invalid(why) => assert!(why.contains("line 3"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_agents_answers_parse() {
        assert_eq!(InGuest::parse("docker-config none"), Some(InGuest::None));
        assert_eq!(
            InGuest::parse("docker-config applied\n"),
            Some(InGuest::Applied)
        );
        assert_eq!(
            InGuest::parse("docker-config unmounted"),
            Some(InGuest::Unmounted)
        );
        assert_eq!(
            InGuest::parse(
                "docker-config refused unable to configure the Docker daemon with file x: bad"
            ),
            Some(InGuest::Refused(
                "unable to configure the Docker daemon with file x: bad".into()
            ))
        );
        assert_eq!(InGuest::parse("lan off"), None);
        assert_eq!(InGuest::parse("docker-config later"), None);
    }

    fn home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "lighter-docker-settings-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(dir(&home)).unwrap();
        home
    }

    /// No file and no settings given: nothing to say.
    #[test]
    fn no_file_no_finding() {
        let home = home("none");
        assert!(finding(&home, Some(InGuest::None)).is_none());
        assert!(finding(&home, None).is_none());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn each_state_has_its_advice() {
        let home = home("states");
        std::fs::write(path(&home), "{}").unwrap();
        std::fs::write(stamp_file(&home), stamp(&home).unwrap()).unwrap();
        let applied = finding(&home, Some(InGuest::Applied)).unwrap();
        assert!(applied.ok && !applied.warn, "{applied:?}");

        let refused = finding(&home, Some(InGuest::Refused(format!("unable to configure the Docker daemon with file {GUEST_PATH}: the following directives are specified both as a flag and in the configuration file: mtu")))).unwrap();
        assert!(
            refused.warn && refused.detail.contains("directives"),
            "{refused:?}"
        );
        assert!(
            !refused.detail.contains(GUEST_PATH) && refused.detail.contains(&shown(&home)),
            "the Mac's path, not the guest's: {refused:?}"
        );

        // Changed since the start: a restart applies it.
        std::fs::write(path(&home), r#"{"debug": true}"#).unwrap();
        let changed = finding(&home, Some(InGuest::Applied)).unwrap();
        assert!(
            changed.warn && changed.detail.contains("changed"),
            "{changed:?}"
        );

        std::fs::write(path(&home), "{ nope").unwrap();
        let invalid = finding(&home, Some(InGuest::None)).unwrap();
        assert!(
            invalid.warn && invalid.detail.contains("not valid JSON"),
            "{invalid:?}"
        );

        std::fs::remove_file(path(&home)).unwrap();
        let gone = finding(&home, Some(InGuest::Applied)).unwrap();
        assert!(gone.warn && gone.detail.contains("gone"), "{gone:?}");
        std::fs::remove_dir_all(home).unwrap();
    }
}
