//! Telling the Docker CLI where to find us.
//!
//! A Docker context is a named endpoint, and registering one is what makes
//! `docker ps` work with nothing exported and nothing to remember. It is done
//! through the `docker` CLI rather than by writing its metadata directory
//! directly: that layout is Docker's to change, and a tool that wrote it by
//! hand would be broken by an upgrade with no warning.

use std::path::Path;
use std::time::{Duration, Instant};

/// The context lighter registers.
pub const NAME: &str = "lighter";

/// How long the context to go back to has to show its daemon is there.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(3);

/// Registers or updates the context, and selects it, remembering in `saved`
/// what was selected before so that `release` can go back to it.
pub fn install(socket: &Path, saved: &Path) -> anyhow::Result<()> {
    if let Some(previous) = to_remember(current()?.as_deref()) {
        std::fs::write(saved, previous)?;
    }
    let endpoint = format!("host=unix://{}", socket.display());
    let exists = list()?.iter().any(|name| name == NAME);
    let verb = if exists { "update" } else { "create" };
    let output = std::process::Command::new("docker")
        .args(["context", verb, NAME, "--docker", &endpoint])
        .arg("--description")
        .arg("lighter")
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "docker context {verb} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let output = std::process::Command::new("docker")
        .args(["context", "use", NAME])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "docker context use failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Points `docker` away from lighter's socket, which is about to go: back to
/// the context selected before `lighter start` if it is still there and its
/// daemon answers, else to `default`. If something else has been selected
/// since, that was someone's choice and is left alone.
pub fn release(saved: &Path) -> anyhow::Result<()> {
    let previous = std::fs::read_to_string(saved).ok();
    let _ = std::fs::remove_file(saved);
    if !owns_selection(current()?.as_deref()) {
        return Ok(());
    }
    let contexts = list()?;
    let target = restore_target(
        previous.as_deref().map(str::trim),
        |name| contexts.iter().any(|c| c == name),
        answers,
    );
    let _ = std::process::Command::new("docker")
        .args(["context", "use", target])
        .output()?;
    Ok(())
}

/// What `install` should remember: the selection, unless it is lighter's own
/// (a start while lighter is selected keeps the earlier memory).
fn to_remember(current: Option<&str>) -> Option<&str> {
    current.filter(|name| *name != NAME)
}

fn owns_selection(current: Option<&str>) -> bool {
    current == Some(NAME)
}

fn restore_target(
    previous: Option<&str>,
    exists: impl Fn(&str) -> bool,
    answers: impl Fn(&str) -> bool,
) -> &str {
    previous
        .filter(|name| !name.is_empty() && *name != NAME && *name != "default")
        .filter(|name| exists(name) && answers(name))
        .unwrap_or("default")
}

/// Whether a context's daemon answers, within `ANSWER_TIMEOUT`: a runtime
/// that has been stopped leaves its context behind, and pointing `docker` at
/// it would fail the same way a vanished lighter socket does.
fn answers(name: &str) -> bool {
    let Ok(mut child) = std::process::Command::new("docker")
        .args([
            "--context",
            name,
            "version",
            "--format",
            "{{.Server.Version}}",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// The context the CLI is currently pointed at.
pub fn current() -> anyhow::Result<Option<String>> {
    let output = std::process::Command::new("docker")
        .args(["context", "show"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok((!name.is_empty()).then_some(name))
        }
        _ => Ok(None),
    }
}

fn list() -> anyhow::Result<Vec<String>> {
    let output = std::process::Command::new("docker")
        .args(["context", "ls", "--format", "{{.Name}}"])
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()),
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_remembers_what_was_selected_unless_it_was_lighter() {
        assert_eq!(to_remember(Some("orbstack")), Some("orbstack"));
        assert_eq!(to_remember(Some("default")), Some("default"));
        assert_eq!(to_remember(Some(NAME)), None);
        assert_eq!(to_remember(None), None);
    }

    #[test]
    fn stop_leaves_a_selection_that_is_not_lighter_alone() {
        assert!(owns_selection(Some(NAME)));
        assert!(!owns_selection(Some("orbstack")));
        assert!(!owns_selection(None));
    }

    #[test]
    fn stop_goes_back_only_to_a_context_that_exists_and_answers() {
        let yes = |_: &str| true;
        let no = |_: &str| false;
        assert_eq!(restore_target(Some("orbstack"), yes, yes), "orbstack");
        assert_eq!(
            restore_target(Some("orbstack"), no, yes),
            "default",
            "removed since"
        );
        assert_eq!(
            restore_target(Some("orbstack"), yes, no),
            "default",
            "its daemon is down"
        );
        assert_eq!(restore_target(None, yes, yes), "default");
        assert_eq!(restore_target(Some(""), yes, yes), "default");
        assert_eq!(restore_target(Some(NAME), yes, yes), "default");
    }
}

/// Against the real docker CLI, in a private `DOCKER_CONFIG` so no real
/// context is touched, with a daemon of its own that answers what `answers`
/// asks (`_ping` and `version`). `cargo test -p lighter-cli context_round_trip
/// -- --ignored`.
#[cfg(test)]
mod live {
    use super::*;

    fn docker(args: &[&str]) {
        let status = std::process::Command::new("docker")
            .args(args)
            .output()
            .unwrap()
            .status;
        assert!(status.success(), "docker {args:?}");
    }

    /// A daemon that answers the docker CLI's ping and version, enough for
    /// it to count as live: a real one needed a machine running beside the
    /// test, named in the environment, and failed without it.
    fn answering_daemon(path: &std::path::Path) {
        use std::io::{BufRead, BufReader, Write};
        let _ = std::fs::remove_file(path);
        let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut writer = stream;
                    loop {
                        let mut request = String::new();
                        if reader.read_line(&mut request).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut line = String::new();
                        while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                            line.clear();
                        }
                        let body = if request.contains("/version") {
                            r#"{"Version":"28.3.3","ApiVersion":"1.51","MinAPIVersion":"1.24","Os":"linux","Arch":"arm64"}"#
                        } else {
                            "OK"
                        };
                        let head = request.starts_with("HEAD ");
                        let reply = format!(
                            "HTTP/1.1 200 OK\r\nApi-Version: 1.51\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n{}",
                            if body == "OK" {
                                "text/plain"
                            } else {
                                "application/json"
                            },
                            body.len(),
                            if head { "" } else { body }
                        );
                        if writer.write_all(reply.as_bytes()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
    }

    #[test]
    #[ignore]
    fn context_round_trip() {
        let dir = std::env::temp_dir().join(format!("lighter-context-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("live.sock");
        answering_daemon(&live);
        let live = live.display();
        // SAFETY: the only test in this binary run that touches the environment.
        unsafe { std::env::set_var("DOCKER_CONFIG", &dir) };
        let saved = dir.join("previous-context");
        docker(&[
            "context",
            "create",
            "other",
            "--docker",
            &format!("host=unix://{live}"),
        ]);
        docker(&[
            "context",
            "create",
            "gone",
            "--docker",
            "host=unix:///nonexistent.sock",
        ]);

        docker(&["context", "use", "other"]);
        install(&dir.join("lighter.sock"), &saved).unwrap();
        assert_eq!(current().unwrap().as_deref(), Some(NAME));
        release(&saved).unwrap();
        assert_eq!(
            current().unwrap().as_deref(),
            Some("other"),
            "back to a live context"
        );

        docker(&["context", "use", "gone"]);
        install(&dir.join("lighter.sock"), &saved).unwrap();
        release(&saved).unwrap();
        assert_eq!(
            current().unwrap().as_deref(),
            Some("default"),
            "not to a dead one"
        );

        install(&dir.join("lighter.sock"), &saved).unwrap();
        docker(&["context", "use", "other"]);
        release(&saved).unwrap();
        assert_eq!(
            current().unwrap().as_deref(),
            Some("other"),
            "a later choice is kept"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
