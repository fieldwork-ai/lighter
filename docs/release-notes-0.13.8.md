# lighter 0.13.8

When the Mac runs out of room for open files, lighter now gives back what it holds, and it never holds a third of the Mac's files to begin with. A Mac whose file table fills fails opens in every program and logs its user out. On the Mac in #95 lighter held about 160,000 of the 491,520 the Mac allows, and gave none of them back when another program filled the rest. Thanks to @petrfilip for the report, the diagnosis and the fix this release builds on (#95, #96).

## What went wrong

**lighter could take a third of the Mac's files** (#95). lighter keeps a descriptor for each file and folder a container has looked at, as a cache, up to three quarters of what macOS lets one process open. But every program's open files come out of one table for the whole Mac, and macOS lets one process open a third to a half of it, so lighter's cache alone could take a third of the table. When another program filled the rest, every open on the Mac failed and the user was logged out.

**When the table was full, lighter gave nothing back.** A container's open failed with "too many open files in system", and lighter kept every descriptor it had. A file a container had just created could be lost: lighter answers a create at once and makes the file a moment later, and when the full table refused it then, no request was left to try again. The log said `an acknowledged create failed to apply errno=23`.

**A quiet share could starve a busy one.** A machine's shares (`/Users`, `/Volumes`, `/var/folders`) count against one budget, but each lets go only of its own descriptors, and only while a container uses it. A share that had filled the budget and gone quiet kept it, and a busy share beside it had to open every file afresh.

## What changed

- **lighter's cache takes a fifth of the Mac's file table at most**: 98,304 on a Mac Studio whose table is 491,520, where it was 161,280. A stock Mac is unchanged, at 6,144.
- **A full table gets room back.** When an open fails because the Mac's table is full, every share lets go of everything it holds idle, and lighter holds half as much until the table has room again: each minute the table is no more than three quarters full, it holds twice as much, back to where it was. A request that only reads is served again, so the container sees a moment's wait. A request that changes something sees the error once, as on a NAS that refuses. `machine.log` says when the table was full and when it had room again.
- **A file a container created is made, even if the table was full at that moment.** lighter lets go of what it holds and makes the file again.
- **A machine's shares share their budget.** A share that holds less than half of it when it is full has the other shares let go of what they have not used lately.
- **A NAS's "too many open files in system" is still the NAS's**, unless the Mac's own table is nearly full, and is handled as in 0.13.7.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- Linux remains **6.18.52**, with guest patches unchanged since 0.13.6. The data epoch remains **1**.
