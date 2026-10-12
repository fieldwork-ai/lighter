# lighter 0.13.3

A Mac running Frigate, or anything else that wakes the machine a few dozen times a second, uses about a quarter less CPU and energy. On a Mac Studio with Frigate watching one camera on 30 virtual CPUs, lighter went from a third of a core to a quarter, and the energy macOS charges it fell by the same share. Nothing that idle time used to speed up got slower.

## What went wrong

**Idle virtual CPUs spun when nothing was coming.** A virtual CPU with nothing to do spins for a moment before it sleeps, because waking a sleeping one costs far more than catching the next task while still awake. lighter keeps that spin short on a Mac whose cores the machine fills, and backs off when spinning stops catching anything. Two things undid that. While a container keeps a stream open to the Mac's Neural Engine or GPU, lighter lets the spin grow so a model's replies are caught at once, and that also lengthened every idle spin from 50 µs to 200, for as long as the stream stayed open, which for Frigate's detector is always. And the back-off was measured in time, ten milliseconds at first, while a CPU woken a few dozen times a second by a camera's frames came back to idle less often than that, so it never noticed it had backed off. Frigate's machine spent about a tenth of a core spinning for nothing.

**Every virtual CPU ran at high priority while a model's stream was open.** A model answering a conversation needs its virtual CPUs woken at once, so lighter raises them to macOS's interactive priority while a container talks to the Neural Engine, the GPU or MPS. It did so for as long as the stream was open, so a detector that answers a few times a second kept the whole machine at that priority all day.

## What changed

- **Idle virtual CPUs rest at their own spin window**, whatever a model's stream asks for while it talks, and **back off by how often they go idle, not by the clock** (guest kernel patch 0053), so a CPU woken a few dozen times a second backs off as surely as a busy one.
- **The virtual CPUs are raised only while a model is actually answering in a loop**, replies every few milliseconds, and drop back two seconds after it stops. A detector a few times a second never raises them.

With Frigate on one camera, three runs each:

| | 0.13.2 | 0.13.3 |
|---|---:|---:|
| lighter's CPU | 0.32–0.34 of a core | 0.25–0.26 |
| energy impact (powermetrics) | 233–248 | 181–189 |
| spinning while idle | about 0.1 of a core | 0.03 |
| detector inference | 7.3–8.0 ms | 7.1–7.5 ms |

Docker DNS lookups in a burst, a reply after a quiet spell, container start and llama.cpp's generation over Metal on an M1 (86–88 tokens a second) are as fast as on 0.13.2.

Frigate's decoding was looked at too: the media engine does it, and most of the CPU that stopping Frigate's decoder saved was Frigate's own work on each frame, inside the machine, not lighter's.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- Linux remains **6.18.52**, with guest patch 0053 (kernel b2387769). The data epoch remains **1**.
