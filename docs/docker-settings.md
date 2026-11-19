# Docker settings

The Docker engine inside lighter's machine reads `~/.lighter/docker/daemon.json`, in Docker's own [`daemon.json` format](https://docs.docker.com/reference/cli/dockerd/#daemon-configuration-file). The file lives in lighter's home on your Mac, so upgrades leave it alone. Create it, then `lighter restart`:

```sh
mkdir -p ~/.lighter/docker
cat > ~/.lighter/docker/daemon.json <<'EOF'
{
  "default-address-pools": [
    { "base": "10.20.0.0/16", "size": 24 }
  ]
}
EOF
lighter restart
```

`lighter start` says `Docker settings from ~/.lighter/docker/daemon.json` when they apply, and `lighter doctor` lists them. After you edit the file, `doctor` says it has changed since the machine started, and `lighter restart` applies it.

## Smaller networks

Docker's default address pools give each new network a /16 or a /20, and run out after about thirty networks. That's quick with Compose projects, macvlan networks and test stacks. The example above gives each new network a /24 from `10.20.0.0/16`, which is room for 256 networks. Pick a range your LAN, VPN and any other Docker host don't use. Networks that already exist keep their subnets; `docker network prune` removes the ones nothing uses.

With direct access on (`lighter config --direct on`), lighter already makes Docker's networks /24s, inside the subnet the Mac reaches containers by. Leave `default-address-pools` out of the file then: setting it in both places is the clash described below.

## What lighter sets itself

lighter starts the engine with some settings of its own, which the file can't set as well: the engine's sockets and `containerd`, the data root and storage driver, the MTU, IPv6 (`ipv6`, `fixed-cidr-v6`, `ip6tables`), the address `host-gateway` resolves to, the seccomp profile, and while direct access is on, `bip` and `default-address-pools`. Docker refuses a file that sets one of these, naming the setting, as it refuses an unknown key or a bad value.

## When Docker refuses the file

A file Docker refuses doesn't cost you Docker. The machine starts the engine again without it, and both `lighter start` and `lighter doctor` show Docker's reason:

```
  warn  docker settings          Docker would not start with ~/.lighter/docker/daemon.json, so it runs without it: unable to configure the Docker daemon with file ~/.lighter/docker/daemon.json: the following directives don't match any configuration option: no-such-option
```

A file that isn't valid JSON is caught on the Mac before the machine boots, with the line and column. Fix the file, then `lighter restart`.
