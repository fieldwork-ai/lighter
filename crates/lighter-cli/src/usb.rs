//! `lighter usb`: USB devices on the Mac, attached to the guest.
//!
//! The configuration is the one record of what is attached: the commands
//! write it, and the running machine reads it when told to (`usb.sock`,
//! `reload`) and at start. The machine attaches a wanted device whenever it
//! is plugged in, and says where each stands (`status`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lighter_vmm::usb::iousb;
use lighter_vmm::usb::manager::{Manager, Spec, Status, Want, refusal};
use serde::{Deserialize, Serialize};

use crate::config::{Config, UsbDevice};

const SOCKET: &str = "usb.sock";
/// The devices the machine holds, for the keeper (`keeper`).
const HELD: &str = "usb-held.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub spec: String,
    /// `attached`, `waiting`, `refused` or `retrying`.
    pub status: String,
    #[serde(default)]
    pub detail: String,
}

fn wants(config: &Config) -> Vec<Want> {
    config
        .usb
        .iter()
        .filter_map(|d| match d.spec.parse::<Spec>() {
            Ok(spec) => Some(Want {
                spec,
                force: d.force,
            }),
            Err(e) => {
                tracing::warn!("usb: ignoring {}: {e}", d.spec);
                None
            }
        })
        .collect()
}

fn entries(manager: &Manager) -> Vec<Entry> {
    manager
        .status()
        .into_iter()
        .map(|(spec, status)| {
            let (status, detail) = match status {
                Status::Attached => ("attached", String::new()),
                Status::Waiting => ("waiting", String::new()),
                Status::Refused(why) => ("refused", why),
                Status::Retrying(why) => ("retrying", why),
            };
            Entry {
                spec: spec.to_string(),
                status: status.into(),
                detail,
            }
        })
        .collect()
}

/// The machine's side: attaches what the configuration asks for, and
/// answers `reload` and `status` on `usb.sock`, one line in, JSON out.
pub struct Server {
    path: PathBuf,
}

impl Server {
    pub fn start(
        home: &Path,
        reactor: Arc<lighter_vmm::reactor::Reactor>,
    ) -> anyhow::Result<Server> {
        let manager = Manager::start(reactor, home.join(HELD))?;
        spawn_keeper();
        manager.set(wants(&Config::load()?));
        let path = home.join(SOCKET);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        std::thread::Builder::new()
            .name("usb-control".into())
            .spawn(move || {
                for connection in listener.incoming() {
                    let Ok(mut stream) = connection else { continue };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    let mut line = String::new();
                    if BufReader::new(&stream).read_line(&mut line).is_err() {
                        continue;
                    }
                    if line.trim() == "reload" {
                        match Config::load() {
                            Ok(config) => manager.set(wants(&config)),
                            Err(e) => tracing::warn!("usb: cannot read the configuration: {e}"),
                        }
                    }
                    let reply =
                        serde_json::to_string(&entries(&manager)).unwrap_or_else(|_| "[]".into());
                    let _ = writeln!(stream, "{reply}");
                }
            })?;
        Ok(Server { path })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Starts the keeper: a process of its own that outlives this one, and gives
/// back to macOS whatever this one held when it exits, however it exits. A
/// seized device is not reset when its holder dies, so a machine killed with
/// its devices attached would otherwise leave them unconfigured and out of
/// macOS's reach until they were unplugged.
fn spawn_keeper() {
    use std::os::unix::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = std::process::Command::new(exe);
    command
        .args(["usb-keeper", "--parent", &std::process::id().to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setsid is async-signal-safe; a session of its own keeps it out
    // of a signal to the machine's process group.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    if let Err(e) = command.spawn() {
        tracing::warn!("usb: no keeper, so a crash would leave attached devices unconfigured: {e}");
    }
}

/// The keeper's whole life: wait for the machine's process to exit, then
/// give back what it held.
pub fn keeper(parent: u32) -> anyhow::Result<std::process::ExitCode> {
    // SAFETY: kqueue() takes nothing.
    let kq = unsafe { libc::kqueue() };
    let change = libc::kevent {
        ident: parent as usize,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let mut event = change;
    // SAFETY: one change and room for one event; a parent already gone
    // fails the registration, which is as good as its exit.
    let n = unsafe { libc::kevent(kq, &change, 1, &mut event, 1, std::ptr::null()) };
    let _ = n;
    lighter_vmm::usb::manager::restore_held(&crate::paths::home()?.join(HELD));
    Ok(std::process::ExitCode::SUCCESS)
}

/// Asks the running machine, if there is one: `reload` or `status`.
fn ask(request: &str) -> Option<Vec<Entry>> {
    let home = crate::paths::home().ok()?;
    let pid = crate::machine::running_pid().ok()??;
    let stream = UnixStream::connect(home.join(SOCKET)).ok()?;
    if crate::instance::Identity::peer(&home, &stream).ok()?.pid() != pid {
        return None;
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut stream = stream;
    writeln!(stream, "{request}").ok()?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply).ok()?;
    serde_json::from_str(&reply).ok()
}

fn speed(s: Option<iousb::Speed>) -> &'static str {
    match s {
        Some(iousb::Speed::Low) => "low speed",
        Some(iousb::Speed::Full) => "full speed",
        Some(iousb::Speed::High) => "high speed",
        Some(iousb::Speed::Super) | Some(iousb::Speed::SuperPlus) => "SuperSpeed",
        None => "",
    }
}

fn name(info: &iousb::Info) -> String {
    match (info.vendor.is_empty(), info.product.is_empty()) {
        (false, false) => format!("{} {}", info.vendor, info.product),
        (true, false) => info.product.clone(),
        (false, true) => info.vendor.clone(),
        (true, true) => "(no name)".into(),
    }
}

pub fn list() -> anyhow::Result<std::process::ExitCode> {
    let config = Config::load()?;
    let status = ask("status");
    let devices = iousb::list();
    println!("USB devices on this Mac:");
    for info in &devices {
        let spec = format!("{:04x}:{:04x}", info.vendor_id, info.product_id);
        let wanted = config
            .usb
            .iter()
            .find(|d| d.spec.parse::<Spec>().is_ok_and(|s| s.matches(info)));
        let state = match (wanted, &status) {
            (Some(w), Some(entries)) => entries
                .iter()
                .find(|e| e.spec == w.spec)
                .map(|e| {
                    if e.detail.is_empty() {
                        e.status.clone()
                    } else {
                        format!("{}: {}", e.status, e.detail)
                    }
                })
                .unwrap_or_else(|| "attaching".into()),
            (Some(_), None) => "attached when the machine starts".into(),
            (None, _) => match refusal(info) {
                Some(why) => format!("not attached ({why})"),
                None => "not attached".into(),
            },
        };
        let serial = if info.serial.is_empty() {
            String::new()
        } else {
            format!(" serial {}", info.serial)
        };
        let port = info
            .callout
            .as_deref()
            .map(|p| format!(" {p}"))
            .unwrap_or_default();
        println!(
            "  {spec}  {}{serial}  {}{port}",
            name(info),
            speed(info.speed)
        );
        println!("             {state}");
    }
    let missing: Vec<&UsbDevice> = config
        .usb
        .iter()
        .filter(|d| {
            !devices
                .iter()
                .any(|i| d.spec.parse::<Spec>().is_ok_and(|s| s.matches(i)))
        })
        .collect();
    if !missing.is_empty() {
        println!("Attached, not plugged in:");
        for d in missing {
            println!("  {}", d.spec);
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

pub fn attach(spec: &str, force: bool) -> anyhow::Result<std::process::ExitCode> {
    let parsed: Spec = spec.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    let present = iousb::list().into_iter().find(|i| parsed.matches(i));
    if let Some(info) = &present
        && !force
        && let Some(why) = refusal(info)
    {
        eprintln!("lighter: {spec} is {why}.");
        return Ok(std::process::ExitCode::FAILURE);
    }
    let mut config = Config::load()?;
    let canonical = parsed.to_string();
    config
        .usb
        .retain(|d| d.spec.parse::<Spec>().ok().as_ref() != Some(&parsed));
    config.usb.push(UsbDevice {
        spec: canonical.clone(),
        force,
    });
    config.save()?;
    let Some(info) = present else {
        println!("{canonical} will be attached whenever it is plugged in.");
        return Ok(std::process::ExitCode::SUCCESS);
    };
    if ask("reload").is_none() {
        println!(
            "{canonical} ({}) will be attached when the machine starts.",
            name(&info)
        );
        return Ok(std::process::ExitCode::SUCCESS);
    }
    // The machine attaches it at once; say how that went.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let entry =
            ask("status").and_then(|entries| entries.into_iter().find(|e| e.spec == canonical));
        match entry {
            // Served, and now in the guest: enumerated, its driver bound,
            // and its names made, which is what a container needs.
            Some(e) if e.status == "attached" && in_guest(info.vendor_id, info.product_id) => {
                println!("{canonical} ({}) is attached.", name(&info));
                let names: Vec<String> = serial_names()
                    .into_iter()
                    .filter(|n| n.vendor == info.vendor_id && n.product == info.product_id)
                    .map(|n| n.path)
                    .collect();
                for path in &names {
                    println!("  docker run --device {path} …");
                }
                return Ok(std::process::ExitCode::SUCCESS);
            }
            Some(e) if e.status == "refused" => {
                eprintln!("lighter: {canonical} is not attached yet: {}.", e.detail);
                if e.detail.contains("is open in") {
                    eprintln!(
                        "It stays attached in the configuration, and the guest gets it as soon as the port is free."
                    );
                }
                return Ok(std::process::ExitCode::FAILURE);
            }
            Some(e) if Instant::now() >= deadline => {
                eprintln!(
                    "lighter: {canonical} is not attached yet ({}: {})",
                    e.status, e.detail
                );
                return Ok(std::process::ExitCode::FAILURE);
            }
            _ => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

pub fn detach(spec: &str) -> anyhow::Result<std::process::ExitCode> {
    let parsed: Spec = spec.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    let mut config = Config::load()?;
    let before = config.usb.len();
    config
        .usb
        .retain(|d| d.spec.parse::<Spec>().ok().as_ref() != Some(&parsed));
    if config.usb.len() == before {
        eprintln!("lighter: {spec} is not attached.");
        return Ok(std::process::ExitCode::FAILURE);
    }
    config.save()?;
    let _ = ask("reload");
    println!("{parsed} is detached; macOS has it back.");
    Ok(std::process::ExitCode::SUCCESS)
}

/// Runs a command in the guest through its control channel, and returns
/// what it printed.
fn guest_sh(command: &str) -> anyhow::Result<String> {
    let home = crate::paths::home()?;
    let mut stream = UnixStream::connect(home.join("control.sock"))
        .map_err(|e| anyhow::anyhow!("the machine is not running ({e})"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    writeln!(stream, "sh {command}")?;
    let mut out = String::new();
    for line in BufReader::new(&stream).lines() {
        let line = line?;
        if line == "--end--" {
            break;
        }
        if !line.starts_with("exit=") {
            out.push_str(&line);
            out.push('\n');
        }
    }
    Ok(out)
}

/// Whether the guest has enumerated a device.
fn in_guest(vendor: u16, product: u16) -> bool {
    let want = format!("{vendor:04x}:{product:04x}");
    guest_sh("for d in /sys/bus/usb/devices/*; do [ -f $d/idVendor ] && echo $(cat $d/idVendor):$(cat $d/idProduct); done")
        .is_ok_and(|out| out.lines().any(|l| l.trim() == want) && serial_settled(vendor, product))
}

/// A serial device is ready once its names are: a device with no serial
/// port is ready as soon as it is enumerated.
fn serial_settled(vendor: u16, product: u16) -> bool {
    let has_tty = guest_sh(&format!(
        "for d in /sys/bus/usb/devices/*; do [ \"$(cat $d/idVendor 2>/dev/null):$(cat $d/idProduct 2>/dev/null)\" = {vendor:04x}:{product:04x} ] && ls -d $d/*/tty* $d/*/ttyUSB* 2>/dev/null; done"
    ))
    .is_ok_and(|out| !out.trim().is_empty());
    !has_tty
        || serial_names()
            .iter()
            .any(|n| n.vendor == vendor && n.product == product)
}

struct SerialName {
    path: String,
    tty: String,
    vendor: u16,
    product: u16,
}

/// Every `/dev/serial/by-id` name in the guest, with its tty and the USB
/// device it belongs to.
fn serial_names() -> Vec<SerialName> {
    let script = "for l in /dev/serial/by-id/*; do [ -e \"$l\" ] || continue; t=$(readlink -f $l); n=${t##*/}; \
                  d=$(readlink -f /sys/class/tty/$n/device); while [ \"$d\" != / ] && [ ! -f $d/idVendor ]; do d=${d%/*}; done; \
                  echo \"$l $n $(cat $d/idVendor 2>/dev/null) $(cat $d/idProduct 2>/dev/null)\"; done";
    let Ok(out) = guest_sh(script) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let (path, tty, vendor, product) = (f.next()?, f.next()?, f.next()?, f.next()?);
            Some(SerialName {
                path: path.to_owned(),
                tty: tty.to_owned(),
                vendor: u16::from_str_radix(vendor, 16).ok()?,
                product: u16::from_str_radix(product, 16).ok()?,
            })
        })
        .collect()
}

/// The guest's `/dev/serial/by-id` names, for `docker run --device`.
pub fn ls_serial() -> anyhow::Result<std::process::ExitCode> {
    guest_sh("true")?;
    let names = serial_names();
    if names.is_empty() {
        println!("No USB serial devices in the guest (`lighter usb list` shows what is attached).");
    }
    for n in names {
        println!(
            "{}  ({}, {:04x}:{:04x})",
            n.path, n.tty, n.vendor, n.product
        );
    }
    Ok(std::process::ExitCode::SUCCESS)
}
