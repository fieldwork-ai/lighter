# lighter 0.13.5

A client that stops reading from a published port no longer silences the machine. In 0.13.4, one such client could stop Docker, every published port and the machine's memory reports until a restart (#84). In 0.13.5 a client that stops reading slows only its own stream.

## What went wrong

**One stalled stream could fill a queue that every stream shares.** Everything the machine sends to the Mac goes through one queue of 256 slots. To keep transfers fast, data stays in its slot until the Mac has taken it. A stream's limit was set in bytes, 4 MiB, but apps often write in 16 KiB pieces; a reverse proxy writing HTTPS does exactly that. 4 MiB in 16 KiB pieces is all 256 slots. So a single client that stopped reading, such as a TV that paused or a player that dropped its stream when seeking, could hold every slot. Docker, every port and the memory reports then had nothing left to send with. When such a client disconnected, lighter closed the stream but kept the data it could no longer deliver, so the slots never came back. The machine looked hung until it was restarted.

**Clients that stop reading could also exhaust the Mac's own network buffers.** lighter gave every network connection on the Mac a fixed 4 MiB buffer, which also switches off macOS's own sizing. A few thousand idle clients then held enough of the Mac's network buffers that writes failed for every connection, and lighter treated that as a dead connection.

## What changed

- **No stream can hold the shared queue.** One stream may hold at most a quarter of it, and the last quarter is never held. Data beyond that is copied out and its slot freed straight away, which on the M5 Ultra costs no measurable speed.
- **Each stream's allowance comes from one shared budget.** When a client stops reading, its allowance shrinks to 64 KiB, so the machine stops sending it what nobody will take. A slow client keeps its allowance and loses nothing.
- **A closed stream gives everything back at once.** If the machine never confirms that a stream has closed, lighter removes it after eight seconds anyway.
- **Docker, the control channel, DNS and the memory reports have their own lane** on both sides, so streams never delay them.
- **A watchdog** resets any stream that holds the shared queue for seconds, says so in `machine.log`, and counts it in `lighter doctor`. The rules above should make this impossible, so `lighter doctor` asks for a report if it ever happens.
- **Connections on the Mac use macOS's own buffer sizing**, which grows a fast connection's buffers to the same 4 MiB and keeps an idle one small. When the Mac does run short of network buffers, lighter waits for them instead of dropping the stream.
- **Transfers into a long-running machine stay fast.** A machine's memory fragments with uptime, especially with services that scan large libraries, and full-size receive buffers then became hard to allocate. They now come from a pool that is reused instead of reallocated, and the pool's spare buffers go back to the Mac after ten seconds of quiet.
- **`lighter status` shows the machine's memory**: how much is available, whether it is waiting on memory, and whether it is fragmented. `lighter doctor` adds a row for it and one for the link between the Mac and the machine.
- **The guest no longer logs a kernel warning** (`invalid nr_accesses_bp`) about two hours after every boot.

The report's failure, recreated on a Mac Studio: a container wrote 8 KiB pieces to a client on the Mac that read nothing. On 0.13.4, `docker ps` and an unrelated port stopped answering within 15 seconds, and stayed that way after the client disconnected. On 0.13.5 both answered within a hundredth of a second throughout, and everything was back once the client disconnected. With two thousand such clients, 0.13.4 stopped answering the unrelated port entirely; 0.13.5 answered all 30 requests.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- Linux remains **6.18.52**, with guest patches 0054 (receive buffers from a pool) and 0055 (the control lane) (kernel 3bea69fa). The data epoch remains **1**.
- `LIGHTER_TCP_WIDEN=1` restores the fixed 4 MiB buffers on the Mac's connections, for comparison.
