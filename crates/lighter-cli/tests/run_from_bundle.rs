//! `lighter run` started outside the bundle (by `brew services`, whose agent
//! names `bin/lighter`) becomes the bundle's executable before anything
//! else, because only the bundle is given Local Network permission (#101).

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn a_bare_run_execs_into_the_bundle() {
    let temp = tempfile::tempdir().unwrap();
    let guest = temp.path().join("share/lighter");
    let macos = guest.join("lighter.app/Contents/MacOS");
    std::fs::create_dir_all(&macos).unwrap();
    // Stands in for the shipped bundle's executable: says what it was run
    // as, with what, and as which process.
    let exe = macos.join("lighter");
    std::fs::write(
        &exe,
        "#!/bin/sh\necho \"pid=$$ args=$* guest=$LIGHTER_GUEST_DIR\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_lighter"))
        .arg("run")
        .env("LIGHTER_HOME", temp.path().join("home"))
        .env("LIGHTER_GUEST_DIR", &guest)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The same process: an exec, so launchd's process becomes the machine.
    let pid = child.id();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("pid={pid} args=run guest={}", guest.display()),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
