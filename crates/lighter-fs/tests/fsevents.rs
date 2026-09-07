//! OS-backed watcher checks run in their own test executable. The unit suite
//! deliberately creates large inode/cache workloads; running these observers
//! concurrently with those writes made correct events arrive after 13.6 s on
//! a loaded host. Keep the original ten-second delivery assertions and test
//! ordinary asynchronous delivery, without forcing the daemon to flush.

mod tests {
    use lighter_fs::fsevents::{Observer, Watcher};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Clone, Debug)]
    struct Observed {
        path: std::path::PathBuf,
        received: std::time::Instant,
        rescan: bool,
    }

    impl Observed {
        fn covers(&self, name: &str, write_completed: std::time::Instant) -> bool {
            if self.rescan {
                self.received >= write_completed
            } else {
                self.path.ends_with(name)
            }
        }
    }

    struct Collector(Arc<Mutex<Vec<Observed>>>);

    // The teardown test intentionally floods the process's FSEvents client.
    // Keep that traffic separate from the delivery/IgnoreSelf assertions.
    static WATCHER_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn a_rescan_must_follow_the_write_it_is_expected_to_cover() {
        let now = std::time::Instant::now();
        let event = Observed {
            path: "/share/ready".into(),
            received: now,
            rescan: true,
        };
        assert!(event.covers("touched", now));
        assert!(!event.covers("touched", now + Duration::from_millis(1)));
        let precise = Observed {
            rescan: false,
            ..event
        };
        assert!(!precise.covers("touched", now));
        assert!(precise.covers("ready", now));
    }

    impl Observer for Collector {
        fn changed(&self, path: &Path) {
            self.0.lock().unwrap().push(Observed {
                path: path.to_path_buf(),
                received: std::time::Instant::now(),
                rescan: false,
            });
        }
        fn rescan(&self, path: &Path, _root_changed: bool) {
            self.0.lock().unwrap().push(Observed {
                path: path.to_path_buf(),
                received: std::time::Instant::now(),
                rescan: true,
            });
        }
    }

    fn watched_root(name: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("lighter-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // FSEvents reports resolved paths, and /var/folders is a symlink.
        std::fs::canonicalize(&root).unwrap()
    }

    fn wait_for(
        seen: &Arc<Mutex<Vec<Observed>>>,
        name: &str,
        write_completed: std::time::Instant,
        within: Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if seen
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.covers(name, write_completed))
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    // Stream startup is asynchronous. Prove that a control event is being
    // delivered before testing a later change; a fixed sleep can lose the
    // only control write before the stream is ready on a loaded host.
    fn wait_until_ready(root: &Path, seen: &Arc<Mutex<Vec<Observed>>>) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            assert!(
                std::process::Command::new("/usr/bin/touch")
                    .arg(root.join("ready"))
                    .status()
                    .unwrap()
                    .success()
            );
            if wait_for(
                seen,
                "ready",
                std::time::Instant::now(),
                Duration::from_millis(100),
            ) {
                return;
            }
        }
        panic!(
            "FSEvents did not become ready; observed {:?}",
            seen.lock().unwrap()
        );
    }

    /// The claim the whole caching policy rests on: a change made on the host,
    /// by something that is not us, is reported quickly enough to be useful as
    /// an invalidation signal.
    #[test]
    fn a_host_change_is_reported() {
        let _guard = WATCHER_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = watched_root("host");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let watcher = Watcher::start(
            &root,
            Duration::from_millis(5),
            Box::new(Collector(seen.clone())),
        )
        .expect("FSEvents should start on any Mac");

        wait_until_ready(&root, &seen);
        // Another process, because our own writes are deliberately invisible.
        assert!(
            std::process::Command::new("/usr/bin/touch")
                .arg(root.join("touched"))
                .status()
                .unwrap()
                .success()
        );

        let found = wait_for(
            &seen,
            "touched",
            std::time::Instant::now(),
            Duration::from_secs(10),
        );
        let paths = seen.lock().unwrap().clone();
        drop(watcher);
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            found,
            "FSEvents supplied neither the changed path nor a rescan after the write: {paths:?}"
        );
    }

    /// The teardown race, run enough times to catch it. A stream torn down
    /// while its callback is mid-flight reads freed memory, and the symptom is
    /// an occasional SIGBUS with no connection to the code that caused it.
    #[test]
    fn a_watcher_can_be_dropped_while_events_are_arriving() {
        let _guard = WATCHER_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for round in 0..20 {
            let root = watched_root(&format!("teardown-{round}"));
            let seen = Arc::new(Mutex::new(Vec::new()));
            let watcher = Watcher::start(
                &root,
                Duration::from_millis(1),
                Box::new(Collector(seen.clone())),
            )
            .unwrap();
            let mut writer = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "for i in $(seq 1 200); do touch {}/f$i; done",
                    root.display()
                ))
                .spawn()
                .unwrap();
            // Dropped with the shell still writing, which is the window.
            drop(watcher);
            let _ = writer.wait();
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// The other half, and the one that makes caching possible at all: writes
    /// *we* make are not reported. The guest's package install reaches the disk
    /// through this process, and if those writes came back as host activity
    /// they would switch off the caching they most need.
    #[test]
    fn our_own_writes_are_not_reported() {
        let _guard = WATCHER_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = watched_root("self");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let watcher = Watcher::start(
            &root,
            Duration::from_millis(5),
            Box::new(Collector(seen.clone())),
        )
        .expect("FSEvents should start on any Mac");

        wait_until_ready(&root, &seen);
        for index in 0..50 {
            std::fs::write(root.join(format!("ours-{index}")), b"x").unwrap();
        }
        // A control, made by another process, so the test can tell "nothing was
        // reported because IgnoreSelf worked" from "nothing was reported
        // because the stream was not running".
        std::process::Command::new("/usr/bin/touch")
            .arg(root.join("theirs"))
            .status()
            .unwrap();

        let control = wait_for(
            &seen,
            "theirs",
            std::time::Instant::now(),
            Duration::from_secs(10),
        );
        let paths = seen.lock().unwrap().clone();
        let ours = paths
            .iter()
            .filter(|event| !event.rescan && event.path.to_string_lossy().contains("ours-"))
            .count();
        drop(watcher);
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            control,
            "the stream was not delivering events at all: {paths:?}"
        );
        assert_eq!(
            ours, 0,
            "{ours} of our own writes came back as host activity"
        );
    }
}
