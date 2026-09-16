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
the 0.5.4 patches, and the data epoch remains **1**.

## Release artifacts

To be recorded at packaging.
