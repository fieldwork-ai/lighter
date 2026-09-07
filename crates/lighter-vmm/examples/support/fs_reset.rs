//! Test-only host writer in the VMM process: IgnoreSelf deliberately hides
//! these writes from FSEvents. The gate can therefore prove that a global
//! reset, rather than an ordinary per-file notification, restores coherence.

use lighter_fs::notify::{Notification, RESETS, Sink};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub fn start(root: PathBuf, sinks: Vec<Arc<Sink>>) {
    std::thread::spawn(move || {
        let mut last = String::new();
        loop {
            let command = std::fs::read_to_string(root.join("command")).unwrap_or_default();
            if command == "stop" {
                break;
            }
            if !command.is_empty() && command != last {
                let result = apply(&root.join("share"), &sinks, command.trim());
                let reply = match result {
                    Ok(()) => command.clone(),
                    Err(error) => format!("ERROR {command}: {error}"),
                };
                let _ = std::fs::write(root.join("ack"), reply);
                last = command;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });
}

fn apply(root: &std::path::Path, sinks: &[Arc<Sink>], command: &str) -> std::io::Result<()> {
    match command {
        "prepare" => {
            std::fs::create_dir_all(root.join("dir"))?;
            for (name, contents) in [
                ("content", "before!"),
                ("mapped", "mappedA"),
                ("empty", ""),
                ("gone", "gone"),
                ("renamed", "rename"),
            ] {
                std::fs::write(root.join(name), contents)?;
            }
            std::fs::write(root.join("truncated"), vec![b'x'; 4096])?;
        }
        "mutate" => {
            for (name, contents) in [("content", "after!!"), ("mapped", "mappedB")] {
                let path = root.join(name);
                let modified = std::fs::metadata(&path)?.modified()?;
                std::fs::write(&path, contents)?;
                std::fs::File::options()
                    .write(true)
                    .open(path)?
                    .set_times(std::fs::FileTimes::new().set_modified(modified))?;
            }
            std::fs::write(root.join("empty"), "filled")?;
            std::fs::write(root.join("truncated"), "")?;
            std::fs::write(root.join("missing"), "exists")?;
            std::fs::write(root.join("dir/new"), "inside")?;
            std::fs::remove_file(root.join("gone"))?;
            std::fs::rename(root.join("renamed"), root.join("renamed-new"))?;
        }
        "reset" => {
            for sink in sinks {
                sink.push(Notification::Reset);
            }
        }
        "overflow" => {
            let before = RESETS.load(Ordering::Relaxed);
            for sink in sinks {
                for nodeid in 0..100_000 {
                    sink.push(Notification::Inode {
                        nodeid: u64::MAX - nodeid,
                    });
                }
            }
            if RESETS.load(Ordering::Relaxed) == before {
                return Err(std::io::Error::other(
                    "probe did not fill the notification queue",
                ));
            }
        }
        _ => return Err(std::io::Error::other("unknown coherence-probe command")),
    }
    Ok(())
}
