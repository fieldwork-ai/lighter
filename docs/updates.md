# Installing and updating Lighter

Lighter 0.5.0 records which installer owns each installation. `lighter doctor`
and `lighter status` identify its location and show installed and running
versions separately. Installing new files does not change a running VM.

## Direct installations

To migrate from 0.4.1, rerun the official installer. Stop the VM first, or pass
`--restart` to explicitly allow the installer to stop it and start it again:

```sh
curl -fsSL https://raw.githubusercontent.com/fieldwork-ai/lighter/main/scripts/install.sh | bash -s -- --restart
```

Once 0.5.0 is installed:

```sh
lighter update check
lighter update download
lighter upgrade
# When the VM is running:
lighter upgrade --restart
```

Check and download do not activate updates. A normal upgrade requires a stopped
VM; `--restart` authorizes interruption of a running VM and its containers.
An initially stopped VM remains stopped. Docker's restart policies determine
which containers come back after a VM restart.

Opt-in daily background downloads are available for direct installations:

```sh
lighter update auto-download on
lighter update auto-download off
```

They default off, run as a separate user launch agent, do not wake the Mac,
and retry failed checks no sooner than one hour later. An available update
appears in interactive CLI notices. Only `lighter upgrade` activates it;
ordinary starts, restarts, login, wake and crash recovery keep the selected
release. Updates are never downloaded in the VMM process.

Each release includes the CLI, VM runtime, guest kernel and root filesystem.
The updater verifies the expected Developer ID, notarization and a signed
manifest covering the external payload before activation. It keeps releases
in separate directories and switches the selected release atomically. Config,
images, volumes and container data remain in the machine home. The previous
runtime is retained for recovery if activation fails; this is not a backup of
container data or a facility for undoing arbitrary future data migrations.

The initial installer downloads a separately signed helper, verifies its
identity before executing it, and delegates archive validation and installation
to that helper. Existing executable wrappers on PATH are preserved.

If an upgrade is interrupted, run it again to recover. If a VM is still
running from the interrupted transaction, stop it before retrying. The recovery
journal is kept until recovery completes. Do not delete release directories
while machines using them are running.

## Homebrew and other installations

Homebrew owns installations in its Cellar:

```sh
brew upgrade fieldwork-ai/tap/lighter
lighter restart
```

Lighter can check for updates, but its direct updater and background downloader
will not replace Homebrew files. Existing Homebrew receipts identify older
installations; 0.5.0's formula also records ownership explicitly. Startup paths
are refreshed when a registered installation starts after an upgrade.

Source builds and unmarked manual installations receive guidance rather than
being silently overwritten. Running the installer explicitly can adopt a
recognized official direct installation. Conflicting metadata or a moved
installation requires repair or reinstallation. Ownership is associated with
an installation, separately from VM configuration, so two installed copies do
not share upgrade authority.

## Kernel policy

0.5.0 retains Linux 6.18.49 and rebuilds it with the kernel match required by
kind's default Service networking. Future kernel and guest updates arrive as part of a
qualified Lighter release, together with the matching host runtime. There is no
independent kernel auto-updater. The current supported release data format is
unchanged; unsupported format epochs are rejected before activation.
