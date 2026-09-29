# lighter 0.10.4

Containers keep their network under load. On 0.10.3 a guest that ran short of threads could lose containers' outbound TCP for good: every connection out of every container was refused until lighter restarted. This release removes the cause, and anything like it now repairs itself.

## What went wrong

Every TCP connection a container makes leaves the guest through one agent process, which carries it to the Mac. That agent gave each connection a thread of its own. Linux caps the threads a machine may have, and sets the cap once at boot from the memory it has then; with cooperative resources a machine boots with 2 GiB, so the cap stayed at about 16,000 while the machine grew to tens of gigabytes.

A heavy browser test pushed the guest to that cap: 18 Chrome instances and thousands of connections through containers' proxies. The next connection's thread could not be created. The agent treated that as fatal and exited, and nothing started it again. DNS kept working, since it is a separate agent, which made the failure look stranger than it was: names resolved, but nothing connected.

## What changed

- **No thread per connection.** Containers' outbound connections, published ports and the Docker socket each run on one event loop that holds every connection as a small state machine. A connection that cannot get what it needs (a descriptor, memory) is refused on its own, and the rest carry on. 20,000 connections out of a container and 10,000 into a published port, all held open at once, leave each loop on its one thread.
- **Agents come back.** If any of the guest's agents exits, it is restarted within a second, and `lighter status` says so: `agents     restarted: tcp-proxy=1 (see lighter logs)`.
- **The thread cap follows the memory.** The guest raises its thread and process caps to the kernel's maximums at boot, so memory and each container's own limits decide, as on any Linux server.
- **The log says what ran out.** When a connection is refused for resources, `lighter logs` has one line a minute naming the resource and the guest's thread and process counts.

## Checked

Gate m3-streams reproduces the failure: it caps the guest's threads just above what is running, opens 300 connections from one container, and restores the cap. On 0.10.3 the agent dies and no container can connect out afterwards, every time; on 0.10.4 all 300 connect. The gate also kills the outbound agent with `kill -9` and checks that containers connect again within 3 seconds and that `lighter status` reports the restart. It runs the floods above, and passes both with sockmap on and with the copying path (`LIGHTER_SOCKMAP=0`).

## If you are on 0.10.3

If containers suddenly cannot connect out while names still resolve, `lighter restart` brings the network back. 0.10.4 makes that unnecessary.

## Also

- Linux remains **6.18.52** (kernel 74880743). The root filesystem is 0.10.3's with the new agent and `init` in place, and nothing else changed; the data epoch remains **1**.
