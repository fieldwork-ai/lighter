# Security

## Reporting a vulnerability

Report a vulnerability privately, through GitHub: [Report a vulnerability](https://github.com/fieldwork-ai/lighter/security/advisories/new), on the repository's Security tab. Please do not open a public issue or pull request for it.

Say what an attacker can do and from where (a container, another user on the Mac, the network), the lighter version, and how to reproduce it. We will acknowledge the report, agree a disclosure date with you, credit you in the advisory unless you would rather we did not, and publish the advisory with the release that fixes it.

## Supported versions

Fixes go into the next release. lighter updates in place (`brew upgrade lighter`, or `lighter update download` then `lighter upgrade --restart`), so only the latest release is supported.

## What counts

lighter's boundary is the virtual machine. Anything that lets code in a container or the guest act on the Mac beyond what lighter gives it (its shared folders, published ports, devices and the Docker socket) is a vulnerability: reading or writing outside a shared folder, reaching the Mac's services, escaping the virtual machine, or running code in lighter's own process. So is a way for another local user, or the network, to reach a machine they were not given. A container reaching what it was deliberately given, such as a shared folder or the Docker socket, is not.
