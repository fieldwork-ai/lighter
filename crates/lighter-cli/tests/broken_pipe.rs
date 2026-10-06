//! A reader that leaves early (`lighter config | head -1`) ends the output,
//! never the command with a panic.

use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Command, Stdio};

#[test]
fn output_into_a_closed_pipe_is_not_a_panic() {
    let mut fds = [0; 2];
    // SAFETY: pipe fills two descriptors, each owned once below.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // Closed before the command starts: its first write fails with EPIPE,
    // with no race against how much a pipe buffers.
    drop(read);
    let home = std::env::temp_dir().join(format!("lighter-broken-pipe-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lighter"))
        .arg("config")
        .env("LIGHTER_HOME", &home)
        .stdout(Stdio::from(write))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    std::fs::remove_dir_all(&home).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(output.status.success(), "{:?}: {stderr}", output.status);
}
