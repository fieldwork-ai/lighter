# lighter 0.5.7

Make a published port answer on `localhost`. A Mac resolves `localhost` to
`::1` first, and Docker publishes a port in both families: its IPv6 mapping
reaches the container's IPv6 address, where a server that binds `0.0.0.0`,
which is most of them, does not listen. Docker refuses, lighter relayed the
refusal, and a dev server that answered on `127.0.0.1` did not answer on
`localhost`. Docker on Linux behaves the same way; this is a deliberate
difference from it.

A v6 publish Docker refuses is now retried by the guest agent on the same port
at the guest's IPv4 address, Docker's v4 mapping of that port, whether Docker
refuses at connect or, through its proxy, by accepting and hanging up before a
byte comes back. A server that answers over v6 is still reached over v6; only a
refusal triggers the retry, and the agent says so once per port in the machine
log. The proper fix in the application, binding `::`, stands regardless and
makes `localhost` work under every Docker.

The stream gate now publishes a server bound to `0.0.0.0` and reads it over
`::1`. Linux remains **6.18.49** with the 0.5.4 patches, and the data epoch
remains **1**.

## Release artifacts

To be recorded at packaging.
