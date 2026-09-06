//! Checking that this Mac can run lighter, and saying what to do if not.
//!
//! Every check here exists because something failed confusingly once. The
//! entitlement one reports `HV_DENIED` from deep inside the hypervisor; the
//! missing-guest one produces a VMM that starts and exits; a stale Docker
//! context makes `docker ps` talk to something that is not running. None of
//! those errors point at their cause, which is what this is for.

use std::fmt::Write as _;

use crate::paths;

pub struct Finding {
    pub ok: bool,
    pub what: String,
    pub detail: String,
    /// What to do about it, when there is something to do.
    pub remedy: Option<String>,
}

impl Finding {
    fn good(what: &str, detail: impl Into<String>) -> Finding {
        Finding {
            ok: true,
            what: what.into(),
            detail: detail.into(),
            remedy: None,
        }
    }

    fn bad(what: &str, detail: impl Into<String>, remedy: &str) -> Finding {
        Finding {
            ok: false,
            what: what.into(),
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }
}

/// Runs every check.
pub fn run() -> Vec<Finding> {
    let mut findings = Vec::new();

    findings.push(if lighter_hv::hv_supported() {
        Finding::good("hardware virtualization", "supported")
    } else {
        Finding::bad(
            "hardware virtualization",
            "kern.hv_support is 0",
            "lighter cannot run inside another virtual machine, and needs Apple Silicon",
        )
    });

    findings.push(match std::env::current_exe() {
        Ok(exe) if is_entitled(&exe) => Finding::good("hypervisor entitlement", "present"),
        Ok(_) => Finding::bad(
            "hypervisor entitlement",
            "the binary is not signed with com.apple.security.hypervisor",
            "reinstall lighter, or run `make sign` in a checkout",
        ),
        Err(e) => Finding::bad("hypervisor entitlement", e.to_string(), "unreadable binary"),
    });

    findings.push(if lighter_vmm::rosetta::installed() {
        match lighter_vmm::rosetta::key() {
            Ok(_) => Finding::good("rosetta", "installed; amd64 containers run under Rosetta"),
            Err(e) => Finding::bad(
                "rosetta",
                format!("installed but not usable: {e}"),
                "amd64 containers will not run until lighter is updated for this Rosetta",
            ),
        }
    } else {
        // Not a fault the machine cannot start with (`start` ignores this
        // finding), but a fault: there is no emulator, so amd64 images fail
        // until Rosetta is installed.
        Finding::bad(
            "rosetta",
            "not installed; amd64 containers will not run",
            "lighter rosetta --install",
        )
    });

    findings.push(match paths::kernel() {
        Ok(path) if path.exists() => Finding::good("guest kernel", path.display().to_string()),
        Ok(path) => Finding::bad(
            "guest kernel",
            format!("missing at {}", path.display()),
            "run `make guest` in a checkout, or reinstall",
        ),
        Err(e) => Finding::bad("guest kernel", e.to_string(), "set LIGHTER_GUEST_DIR"),
    });

    findings.push(match paths::rootfs() {
        Ok(path) if path.exists() => Finding::good("guest filesystem", path.display().to_string()),
        Ok(path) => Finding::bad(
            "guest filesystem",
            format!("missing at {}", path.display()),
            "run `make guest` in a checkout, or reinstall",
        ),
        Err(e) => Finding::bad("guest filesystem", e.to_string(), "set LIGHTER_GUEST_DIR"),
    });

    findings.push(match which("docker") {
        Some(path) => Finding::good("docker client", path),
        None => Finding::bad(
            "docker client",
            "not on PATH",
            "install it with `brew install docker` — lighter is the daemon, not the CLI",
        ),
    });

    findings.push(match free_space_gib() {
        Some(gib) if gib >= 10 => Finding::good("disk space", format!("{gib} GiB free")),
        Some(gib) => Finding::bad(
            "disk space",
            format!("{gib} GiB free"),
            "images and volumes live in ~/.lighter; ten gigabytes is a sensible floor",
        ),
        None => Finding::good("disk space", "unknown"),
    });

    findings.push(match crate::machine::running_pid() {
        Ok(Some(pid)) => Finding::good("machine", format!("running, pid {pid}")),
        Ok(None) => Finding::good("machine", "not running"),
        Err(e) => Finding::bad("machine", e.to_string(), "check ~/.lighter"),
    });

    // A custom home never owns the global context; its machine is reached
    // by DOCKER_HOST, and telling someone to `docker context use lighter`
    // would point them at a machine this doctor is not examining.
    if crate::paths::is_default_home() {
        findings.push(match crate::context::current() {
            Ok(Some(name)) if name == crate::context::NAME => {
                Finding::good("docker context", format!("{name} (selected)"))
            }
            Ok(Some(name)) => Finding::bad(
                "docker context",
                format!("{name} is selected, not {}", crate::context::NAME),
                "run `lighter start`, or `docker context use lighter`",
            ),
            Ok(None) => Finding::bad("docker context", "not registered", "run `lighter start`"),
            Err(e) => Finding::bad("docker context", e.to_string(), "check the docker CLI"),
        });
    } else {
        findings.push(Finding::good(
            "docker context",
            "custom home; reached by DOCKER_HOST, context not touched",
        ));
    }

    findings
}

/// Formats the findings the way `lighter doctor` prints them.
pub fn report(findings: &[Finding]) -> String {
    let mut out = String::new();
    for finding in findings {
        let mark = if finding.ok { "ok  " } else { "FAIL" };
        let _ = writeln!(out, "  {mark}  {:<24} {}", finding.what, finding.detail);
        if let Some(remedy) = &finding.remedy {
            let _ = writeln!(out, "        {remedy}");
        }
    }
    out
}

/// Whether a binary carries the hypervisor entitlement.
///
/// Asked of `codesign` rather than parsed out of the Mach-O: the entitlement
/// only counts if the signature is valid, and validating a signature by hand is
/// not something to reimplement for a diagnostic.
fn is_entitled(path: &std::path::Path) -> bool {
    let Ok(output) = std::process::Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(path)
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.contains("com.apple.security.hypervisor")
}

fn which(program: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .map(|found| found.display().to_string())
}

fn free_space_gib() -> Option<u64> {
    let home = paths::home().ok()?;
    let target = if home.exists() {
        home
    } else {
        std::path::PathBuf::from(std::env::var("HOME").ok()?)
    };
    let c = std::ffi::CString::new(target.to_string_lossy().as_bytes()).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid path and an output buffer we own.
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail * u64::from(st.f_bsize) / (1 << 30))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_names_a_remedy_for_every_failure() {
        let findings = vec![
            Finding::good("fine", "yes"),
            Finding::bad("broken", "no", "do this"),
        ];
        let text = report(&findings);
        assert!(text.contains("ok    fine"));
        assert!(text.contains("FAIL  broken"));
        assert!(
            text.contains("do this"),
            "a failure with no remedy is a diagnostic that helps nobody"
        );
    }

    /// Every check this build makes must offer a remedy when it fails, because
    /// the whole point is to turn a confusing error into an instruction.
    #[test]
    fn every_check_can_say_what_to_do() {
        for finding in run() {
            if !finding.ok {
                assert!(
                    finding.remedy.is_some(),
                    "{} failed with no remedy",
                    finding.what
                );
            }
        }
    }
}
