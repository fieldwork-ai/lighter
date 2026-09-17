# lighter 0.5.6

Smooth the memory balloon's response to host memory pressure. A Mac short of
memory reports pressure in levels, and those levels flap: a host at its limit
was seen switching between warning and normal eighteen times in half an hour.
0.5.5 answered each switch in full, asking the guest for a quarter of its RAM
in one step and handing the whole balloon back the moment the level read
normal, so the guest hunted for the memory, refilled its cache, and was asked
again a minute later, a comb of load spikes on the host.

The balloon is now one ramp. The pressure level raises how far it may go (an
eighth of the guest with no pressure, a quarter at warning, half at critical)
and how fast (a level is reached in about eight seconds), and sets no target
of its own. It comes down only once the host has been quiet for five seconds
with no pressure reported, a 256th of the guest a second, and a change of
level restarts those five seconds. A level that drops is a plateau that eases,
not a cliff, and a flapping host is not answered with a deflate on every
normal second. The guest gives its cold cache first, at a rate it can answer.

The 0.5.5 hold remains for a guest that is truly short: its agent's `need`
line, an inflation that makes under 4 MiB of progress in three seconds, or its
agent gone freeze the ramp at what the balloon has. The agent's `release` line
withdraws the ramp when the host is not short, as it always did, and is no
longer read as shortness under pressure, where inflation itself keeps free
memory low while the guest still has gigabytes of cache to give.

The memory gate now checks the climb from cache (no failed inflations), the
plateau on normal, and three warning/normal flaps leaving the balloon where it
is, beside the short-guest checks from 0.5.5. Linux remains **6.18.49** with
the 0.5.4 patches, and the data epoch remains **1**. The root filesystem was
rebuilt on the release commit: of its 102 Alpine packages only `tzdata` moved,
2026c to 2026d; init scripts, dockerd, containerd and runc are byte-identical
to 0.5.5's, and the agent differs only by its package version.

## Release artifacts

Packaged source: `72c99a9` on `release/0.5.6` (the balloon ramp, the version
bump, and the formula template aligned with the tap). Apple accepted
notarization `25aa6df7-701e-4c71-94e8-d21883597f5a`; the app ticket is stapled
and Gatekeeper accepts the archive's app.

| Artifact | SHA256 |
| --- | --- |
| `lighter-0.5.6-arm64.tar.gz` | `c9d2af42eab25ae10b0cc1b084fdd1f5d5c34db992fcc3ee4ff70d751f9f372a` |
| `lighter-0.5.6-arm64` bootstrap | `b3ec479e743f4b0949804d030b83d306a7375f62aedfca1006d57856aa8bb355` |
| Guest kernel | `33c72dd4987331679097b0fa360f57beb621c4ee10ec111eb3064eeaaf517a97` |
| Guest root filesystem | `fc8146d02580f2c04dfe07d699f550f61bcf7742da7ac0a968335c5e43cc2bb9` |

The kernel is byte-identical to 0.5.4's and 0.5.5's. Final release metadata
may follow the packaged source; runtime, guest and build inputs remain
identical.
