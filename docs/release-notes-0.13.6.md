# lighter 0.13.6

Clients that leave, sit idle, stop reading or hang no longer cost the machine more than their share. A client that closed while its server said nothing was held for good. Idle connections could slow every busy stream to a fraction of its speed. Clients that stopped reading cost 5.6 MiB of memory each. And a hung network server could stall the local files of the folder it was mounted in. Thanks to @petrfilip for diagnosing two of these (#87, #88).

## What went wrong

**A client that closed while its server stayed silent was never let go** (#87). When a client finishes sending, it closes its side of the connection. Some clients then wait for the rest of the reply, as `docker run` does, and some have simply left. lighter couldn't tell the two apart from the end of the stream alone, so it kept every such connection open until the server spoke or closed. A server that did neither, such as a `docker exec` of a quiet process, held the connection for as long as the server lived, and on the Docker socket two threads with it.

**Idle connections slowed busy ones.** Every connection was given its full share of a budget the moment it opened, whether or not it ever sent anything, so the first few dozen connections took all of it. And each time anything happened, lighter looked at every connection to see what had changed. Beside a thousand idle connections, a busy stream ran at a quarter of its speed, and at a twenty-fifth once those connections had closed with their server silent.

**Every connection event woke every waiting connection** (#88). The senders of every connection waited on one shared signal, so each event on any connection woke all of them to find nothing to do.

**A client that stopped reading cost the machine 5.6 MiB of memory.** When a container streams to a client on the Mac that stops reading, the data waits inside the machine for the Mac to take it, and up to 4 MiB of it a stream waited in a queue that nothing else bounded. Two thousand such clients ran a 4 GiB machine out of memory.

**A hung network server could stall the local files of the folder it was mounted in.** Requests to a network volume that has stopped answering wait for it, and each one holds a place in the queues its shared folder has with the Mac. A thousand of them filled the queues, and a file on the Mac's own disk in the same folder then waited with them: 11 minutes in a test, until the server came back.

## What changed

- **A client that has left is let go**, within 5 seconds on the Docker socket and within about a minute and a quarter over a published port. A client that only closed its sending side keeps waiting for its reply for as long as the server takes. A client that vanished, such as a laptop gone to sleep on the network, is let go within about 30 seconds of its connection falling quiet.
- **Idle connections cost busy ones nothing.** A connection's share of the budget starts small and grows as it moves data, and lighter now looks only at the connections something happened to. Beside a thousand idle connections, a busy stream keeps its full speed.
- **An event wakes only its own connection.**
- **A stream into the Mac holds no more inside the machine than the Mac lets it send.** Streams the Mac is taking keep their full speed, and a client that stops reading now costs the machine about a megabyte.
- **A shared folder's queues are never all held by a hung server.** Once a network volume hasn't answered for 30 seconds while every request to it waits, or for 5 seconds once its requests fill three quarters of a queue, the requests to it that haven't started fail with an input/output error instead of waiting, and `machine.log` says so. Nothing those requests asked for has happened. The first answer from the server ends it, and a server that is only slow keeps answering, so nothing to it fails.
- **A rare case could hold a stream paused for good.** When the machine couldn't take a stream's data at once and retried later, its count of waiting bytes went wrong, and the stream never resumed. Every byte is counted now.

Throughput is unchanged: the container-to-Mac and published-port paths measured level with 0.13.5 on a Mac Studio.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- Linux remains **6.18.52**, with guest patch 0056 (a stream's queue into the Mac follows what the Mac lets it send). The data epoch remains **1**.
