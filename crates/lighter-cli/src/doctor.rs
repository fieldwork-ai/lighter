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
    /// Worth doing, not blocking: `ok` for the exit code and for `lighter
    /// start`, printed as a warning with its remedy.
    pub warn: bool,
}

impl Finding {
    fn good(what: &str, detail: impl Into<String>) -> Finding {
        Finding {
            ok: true,
            what: what.into(),
            detail: detail.into(),
            remedy: None,
            warn: false,
        }
    }

    fn bad(what: &str, detail: impl Into<String>, remedy: &str) -> Finding {
        Finding {
            ok: false,
            what: what.into(),
            detail: detail.into(),
            remedy: Some(remedy.into()),
            warn: false,
        }
    }
    /// Something the user should fix that nothing here can refuse to run
    /// over: a permission that macOS grants only to a machine that is
    /// running and has asked, for one.
    fn warn(what: &str, detail: impl Into<String>, remedy: &str) -> Finding {
        Finding {
            ok: true,
            what: what.into(),
            detail: detail.into(),
            remedy: Some(remedy.into()),
            warn: true,
        }
    }
}

/// Runs every check.
pub fn run() -> Vec<Finding> {
    let mut findings = Vec::new();
    findings.push(match crate::installation::current() {
        Ok(_) => Finding::good("installation", crate::installation::describe()),
        Err(error) => Finding::bad(
            "installation",
            format!("conflicting metadata: {error}"),
            "repair this installation using its original installer before updating",
        ),
    });
    findings.push(Finding::good(
        "release versions",
        crate::installation::version_report().trim(),
    ));

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

    // The accelerator devices: each is on when its host component is in
    // the binary (the GPU, the Neural Engine, ggml) or on the Mac (torch).
    let config = crate::config::Config::load().unwrap_or_default();
    findings.push(
        match (config.gpu, lighter_vmm::virtio::gpu::virgl::linked()) {
            (false, _) => Finding::good("lighter.sh/gpu", "off in the configuration"),
            (true, true) => {
                Finding::good("lighter.sh/gpu", "Vulkan in containers, on the Mac's GPU")
            }
            (true, false) => Finding::bad(
                "lighter.sh/gpu",
                "this build has no renderer",
                "run `make gpu` and rebuild, or reinstall a release build",
            ),
        },
    );
    findings.push(
        match (config.ane, lighter_vmm::ane::ort::Runtime::linked()) {
            (false, _) => Finding::good("lighter.sh/ane", "off in the configuration"),
            (true, true) => Finding::good("lighter.sh/ane", "the Neural Engine, for ONNX models"),
            (true, false) => Finding::bad(
                "lighter.sh/ane",
                "this build has no ONNX Runtime",
                "run `make ane` and rebuild, or reinstall a release build",
            ),
        },
    );
    findings.push(match (config.metal, lighter_vmm::metal::linked()) {
        (false, _) => Finding::good("lighter.sh/metal", "off in the configuration"),
        (true, true) => Finding::good(
            "lighter.sh/metal",
            "ggml on the Mac's GPU, for llama.cpp and friends",
        ),
        (true, false) => Finding::bad(
            "lighter.sh/metal",
            "this build has no ggml",
            "run `make metal` and rebuild, or reinstall a release build",
        ),
    });
    findings.push(if config.video {
        Finding::good(
            "lighter.sh/video",
            "H.264 decode in containers, on the Mac's media engine (V4L2, /dev/video0)",
        )
    } else {
        Finding::good("lighter.sh/video", "off in the configuration")
    });
    findings.push(if !config.mps {
        Finding::good("lighter.sh/mps", "off in the configuration")
    } else {
        match crate::mps::find_python(&config.torch_python) {
            Ok((python, version)) => Finding::good(
                "lighter.sh/mps",
                format!(
                    "PyTorch on the Mac's GPU: torch {version} in {}",
                    python.display()
                ),
            ),
            // Not a fault: torch on the Mac is the user's choice, and without
            // it the device is simply absent. Said as a finding that passes,
            // with the way to turn it on.
            Err(why) => Finding::good(
                "lighter.sh/mps",
                format!(
                    "absent: no Python with torch and MPS ({why}); pip install torch in a Python on PATH, or `lighter config --torch-python <path>`"
                ),
            ),
        }
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

    // Only a connect made by the machine's own process tests the machine's
    // own Local Network permission: the gateway is exempt and a shell is
    // not subject, so a check from here against the router would pass while
    // every camera, printer and NAS failed.
    findings.push({
        let pid = paths::home()
            .ok()
            .and_then(|home| crate::instance::Identity::read(&home).ok().flatten())
            .map(|identity| identity.pid());
        let (ok, detail, remedy) = match paths::home() {
            Ok(home) => crate::localnet::doctor_finding(&home, pid),
            Err(e) => (true, format!("untested: {e}"), None),
        };
        // A warning, never a failure: `lighter start` must not refuse over
        // a permission macOS grants only to a machine that is running and
        // has asked, and a gate's fresh machine is a fresh identity.
        match remedy {
            Some(remedy) if !ok => Finding::warn("local network", detail, &remedy),
            _ => Finding::good("local network", detail),
        }
    });

    // The loop that gives a working machine's idle cache back to the Mac
    // (`guest/agent/src/warm.rs`). A machine at 12 GiB "while idle" is the
    // report this row exists to answer in one read: whether the loop runs,
    // how hard the Mac is asking, and how often the guest's own stall held
    // it. An agent that predates the loop answers with an error: no row.
    if matches!(crate::machine::running_pid(), Ok(Some(_)))
        && let Some(finding) = crate::machine::control("warm")
            .ok()
            .as_deref()
            .and_then(warm_finding)
    {
        findings.push(finding);
    }

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

/// The agent's `warm` line, as a row. `None` for anything that is not one.
fn warm_finding(line: &str) -> Option<Finding> {
    let mut words = line.split_whitespace();
    if words.next()? != "warm" {
        return None;
    }
    let state = words.next()?;
    let field = |name: &str| -> Option<u64> {
        line.split_whitespace()
            .find_map(|w| w.strip_prefix(name)?.strip_prefix('='))?
            .parse()
            .ok()
    };
    Some(match state {
        "on" => {
            let (periods, paused) = (field("periods")?, field("paused")?);
            Finding::good(
                "idle cache",
                format!(
                    "{} MiB returned since start; the Mac's need is {} of 20; the guest's own stall held it {} of {} periods",
                    field("reclaimed_mib")?,
                    field("gain")?,
                    paused,
                    periods,
                ),
            )
        }
        "no-psi" => Finding::warn(
            "idle cache",
            "not returned while containers run: the guest has no pressure stall information",
            "remove `psi=0` from the guest's command line",
        ),
        _ => Finding::good("idle cache", "kept while containers run (`lighter.warm=0`)"),
    })
}

/// Formats the findings the way `lighter doctor` prints them.
pub fn report(findings: &[Finding]) -> String {
    let mut out = String::new();
    for finding in findings {
        let mark = if !finding.ok {
            "FAIL"
        } else if finding.warn {
            "warn"
        } else {
            "ok  "
        };
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
    fn the_agents_warm_line_becomes_a_row() {
        let on = warm_finding("warm on gain=8 some_ppm=120 step_kib=4096 periods=400 paused=3 reclaimed_mib=1234 compactions=2").unwrap();
        assert!(on.ok && !on.warn);
        assert!(
            on.detail.contains("1234 MiB")
                && on.detail.contains("8 of 20")
                && on.detail.contains("3 of 400"),
            "{}",
            on.detail
        );
        let blind = warm_finding("warm no-psi gain=1 some_ppm=0 step_kib=0 periods=0 paused=0 reclaimed_mib=0 compactions=0").unwrap();
        assert!(blind.warn && blind.remedy.is_some());
        assert!(warm_finding("warm off gain=1").is_some());
        // An agent from before the loop, and anything else, is no row.
        assert!(warm_finding("error unknown").is_none());
        assert!(warm_finding("").is_none());
    }

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
