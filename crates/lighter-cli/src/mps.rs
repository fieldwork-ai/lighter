//! The PyTorch device's host half: the Mac's own `torch`, in the user's own
//! Python, serving a container's `torch` one operator at a time.
//!
//! Nothing is bundled. The server is a Python module carried in this binary
//! and written out at start; the interpreter is the configured one or the
//! first `python3` on PATH whose torch has MPS. Rosetta is the precedent: use
//! what is on the Mac, and say clearly when it is not there.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const SERVER: &str = include_str!("../../../guest/torch-mps/host/lighter_mps_host.py");

pub struct Host {
    child: Child,
    port: u16,
}

/// A Python that imports torch with MPS, or why none does.
pub fn find_python(configured: &str) -> Result<(PathBuf, String), String> {
    let candidates: Vec<PathBuf> = if configured.is_empty() {
        std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .map(|d| d.join("python3"))
                    .filter(|p| p.exists())
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![PathBuf::from(configured)]
    };
    if candidates.is_empty() {
        return Err("no python3 on PATH".into());
    }
    let mut reasons = Vec::new();
    for python in candidates {
        let out = Command::new(&python)
            .args([
                "-c",
                "import torch; print(torch.__version__, torch.backends.mps.is_available())",
            ])
            .stderr(Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let mut words = text.split_whitespace();
                let version = words.next().unwrap_or("").to_string();
                if words.next() == Some("True") {
                    return Ok((python, version));
                }
                reasons.push(format!("{}: torch {version} without MPS", python.display()));
            }
            _ => reasons.push(format!("{}: no torch", python.display())),
        }
    }
    Err(reasons.join("; "))
}

impl Host {
    /// Starts the server, on a port of its choosing, and reads that port back.
    pub fn start(python: &Path, state_dir: &Path) -> std::io::Result<Host> {
        std::fs::create_dir_all(state_dir)?;
        let script = state_dir.join("lighter_mps_host.py");
        std::fs::write(&script, SERVER)?;
        let mut child = Command::new(python)
            .arg(&script)
            .arg("--port")
            .arg("0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdout = child.stdout.take().expect("piped");
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line)?;
        let port: u16 = line
            .trim()
            .strip_prefix("PORT ")
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| {
                std::io::Error::other(format!("the mps host did not report a port: {line:?}"))
            })?;
        Ok(Host { child, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
