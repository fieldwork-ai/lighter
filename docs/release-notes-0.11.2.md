# lighter 0.11.2

With `resources: cooperative`, a container at its own memory limit no longer makes lighter take more and more of your Mac's memory, and nothing grows at all while macOS says memory is critical.

## What went wrong

A cooperative machine starts small and grows when the Linux inside it runs short. lighter judged "short" partly from how long programs in the machine were waiting on memory. A container that has its own memory limit, such as a `docker buildx` builder made with `--memory 12g`, makes its programs wait in exactly the same way when it reaches that limit, however much memory the machine has free. lighter read that as the machine being short, and kept adding memory it could never use:

- On a 32 GB Mac, two builds in a 12 GiB builder grew a machine from 28 to 47 GiB in seven minutes while macOS reported memory as critical. The Mac swapped until it was unusable, and the builder then ran out of memory within its limit anyway, as it would have without any of that.
- On an 8 GB Mac, one container thrashing at a 1 GiB limit grew the machine to its whole size in a second.

The same misreading also stopped lighter taking memory back from the machine while it lasted.

## What changed

- **Waiting on memory counts only when the machine itself is short.** Linux counts separately the times the whole machine had to stop and free memory for an allocation, and never counts a container reclaiming within its own limit. lighter now grows the machine for waiting only when that count moves. A container at its limit is left to its limit, as on any Linux server: it slows down, and if it cannot fit, Linux stops the process inside it.
- **Nothing grows while macOS says memory is critical,** or for a minute after, however the level moves in between. Work that needs more then waits, and inside the machine Linux frees memory or stops a process in a container, rather than your Mac swapping for it. At macOS's warning level, growth stays paced, as before.
- **The containers' own throttle reads only its own events.** A container with a soft memory limit of its own (`memory.high`) is no longer taken for lighter's throttle.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script. Containers with a restart policy come back by themselves; start the others again, as after any restart.

## Also

- Linux remains **6.18.52** (kernel 7ed412d7). The root filesystem is 0.11.1's with the new agent in place and nothing else changed (rootfs e88e5688); the data epoch remains **1**.
- Machines with fixed resources, the default, are unchanged.
