# lighter 0.5.7

Make a published port answer on `localhost`. A Mac resolves `localhost` to
`::1` first, and Docker publishes a port in both families: its IPv6 mapping
reaches the container's IPv6 address, where a server that binds `0.0.0.0`,
which is most of them, does not listen. Docker refuses, lighter relayed the
refusal, and a dev server that answered on `127.0.0.1` did not answer on
`localhost`. Docker on Linux behaves the same way; this is a deliberate
difference from it.

A v6 publish Docker refuses is now retried by the guest agent on the same port
at the guest's IPv4 address, Docker's v4 mapping of that port, whether Docker
refuses at connect or, through its proxy, by accepting and hanging up before a
byte comes back. A server that answers over v6 is still reached over v6; only a
refusal triggers the retry, and the agent says so once per port in the machine
log. The proper fix in the application, binding `::`, stands regardless and
makes `localhost` work under every Docker.

The stream gate now publishes a server bound to `0.0.0.0` and reads it over
`::1`. Linux remains **6.18.49** with the 0.5.4 patches, and the data epoch
remains **1**.

## Release artifacts

Packaged source: `852ef82` on `release/0.5.7` (the v6 publish fallback and the
version bump). Apple accepted notarization
`4abfcfb8-e3cd-4aee-97d1-f6ab13b2be73`; the app ticket is stapled and
Gatekeeper accepts the archive's app.

| Artifact | SHA256 |
| --- | --- |
| `lighter-0.5.7-arm64.tar.gz` | `8c6794284e0616c5a9497d658d7242c3fb80bfdb319e3d8d8aa828a73b5fc6ca` |
| `lighter-0.5.7-arm64` bootstrap | `8cdbab3a451c828839d1562ddc0d7df0ea520cfe7cac394ee276e6c83b6185ff` |
| Guest kernel | `33c72dd4987331679097b0fa360f57beb621c4ee10ec111eb3064eeaaf517a97` |
| Guest root filesystem | `0c7b475746d2f5300d29d49459792bfdbb0db869e4e0356787281d6b58fa85bb` |

The kernel is byte-identical to 0.5.4's through 0.5.6's. The root filesystem
differs from 0.5.6's only in the guest agent: the same 102 packages at the same
versions, and the init scripts, dockerd, containerd and runc are byte-identical.
Final release metadata may follow the packaged source; runtime, guest and
build inputs remain identical.
