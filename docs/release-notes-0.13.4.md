# lighter 0.13.4

`lighter restart` no longer fails with `Invalid argument (os error 22)` after a machine that had stopped responding is stopped forcibly, and a machine whose link to the Mac stalls now records what the Mac side was doing at the time, so the stall can be traced (#84).

## What went wrong

**A forced stop was reported finished before the machine had let go.** `lighter stop` asks the machine to shut down, and stops it forcibly if it has not done so in 30 seconds. It then waited only until macOS stopped listing the process, but macOS goes on holding a killed process's files until it has freed its memory: a fifth of a second for a machine using 10 GiB, longer on a Mac that is already struggling. `lighter start`, which `restart` runs straight after, found the machine's home still held, took that for a machine from before lighter 0.4.0, and asked the dying process who it was. macOS answered `Invalid argument`, and lighter showed that bare error and stopped. The same happened when a machine was killed some other way and `lighter start` ran within that moment.

**A stall in the link between the Mac and the machine left no trace of its cause.** The report's machine stopped answering Docker, port forwarding and outbound connections at once, while its console kept writing to the log. Everything that stopped travels over the one link between the Mac and the machine, so the stuck part was on the Mac's side of it, but the log could not say where. The machine notices such a stall itself after a few minutes and writes its own state to the console, but that report was too long for the kernel's buffer and most of it was lost.

## What changed

- **A stop waits until the machine has fully let go**, and a start that finds a previous machine still exiting says so and waits for it, up to a minute, instead of failing. A machine that has exited is never mistaken for one from before 0.4.0 again.
- **When the machine reports that the Mac has stopped answering, lighter records every one of its own threads**, with three seconds of macOS's `sample`, and the state of its virtual devices, into `machine.log`. If your machine stalls like this, the log you attach to an issue now shows which part of lighter was stuck.
- **The machine's own report is shorter**: its memory summary and the tasks that are waiting, without every task's stack, which overflowed the kernel's buffer and took the rest of the report with it.

Restarting a 10-core, 12 GiB machine with 9 GiB in use and not responding, on a Mac Studio: 0.13.3 failed with `Invalid argument (os error 22)` every time, as in the report, and 0.13.4 stopped and started it again. Killing the same machine and starting it at once: 0.13.3 failed the same way, and 0.13.4 waited for the old machine to exit and started.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- Linux remains **6.18.52**, with guest patch 0053 (kernel b2387769). The data epoch remains **1**.
