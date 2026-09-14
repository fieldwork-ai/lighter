# lighter 0.5.5

Fix a livelock when macOS reports memory pressure while the guest has no memory
to spare. The memory policy asks the guest for a quarter of its RAM at the
warning level and half at critical, and it did so in one step regardless of
what the guest could give. A guest running a build near its limit could not
give it: its balloon driver retried the failing allocation five times a
second, each try a reclaim pass on a guest with nothing left to reclaim, until
Docker and the guest agent stopped answering. A defect report of 14 September
2026 showed this on a 16 GiB guest running a build bounded at 10 GiB.

A pressure floor is now held at what the balloon already has while the guest
says it is short of memory, or while an inflation makes under 4 MiB of progress
in three seconds, which is what a guest too starved to say anything looks like
from the host. It resumes five seconds after the guest says it is fine. In the
short regime the floor returned nothing to the host anyway; what goes away is
the livelock. A host under real pressure still gets everything the guest can
give, as soon as it can give it.

The memory gate now raises a warning against a guest holding most of its RAM
and checks that Docker answers, the container keeps working, the balloon does
not grow, nothing fails to inflate, and the floor takes its quarter once the
container has gone. Linux remains **6.18.49** with the 0.5.4 patches, and the
data epoch remains **1**.

## Release artifacts

Packaged source: `483b1cd` on `release/0.5.5` (the pressure-floor hold plus the
version bump). Apple accepted notarization
`5a2987cc-e28b-40b7-8968-3b4a79603757`; the app ticket is stapled and
Gatekeeper accepts the archive's app.

| Artifact | SHA256 |
| --- | --- |
| `lighter-0.5.5-arm64.tar.gz` | `7fbc1e77b0140bdb1bd20955ff985fa5f951fc20164b3e8e606c23d34684fe9d` |
| `lighter-0.5.5-arm64` bootstrap | `c8bbafb25ff59826719a0111d9c4efaac4abd976c237265a177121b30a2f71a6` |
| Guest kernel | `33c72dd4987331679097b0fa360f57beb621c4ee10ec111eb3064eeaaf517a97` |
| Guest root filesystem | `ae6ed9e453efa7c901608c3375c1f5e8aff96650db690dc7c2501be9e04c6b07` |

The kernel is byte-identical to 0.5.4's. The root filesystem was rebuilt on
the release commit: its Alpine package inventory, init scripts, dockerd,
containerd and runc are byte-identical to 0.5.4's, and the agent differs only
by its package version. Final release metadata may follow the packaged source;
runtime, guest and build inputs remain identical.
