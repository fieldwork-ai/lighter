# lighter 0.12.1

A container that uses a USB stick, such as Zigbee2MQTT or Z-Wave JS, now comes back by itself after `lighter restart` or an upgrade.

## What went wrong

When lighter restarts, Docker starts every container with a restart policy as soon as it is up, and the Mac attaches the USB devices you hold with `lighter usb attach` a few seconds later. A container that names a stick with `devices:` (`/dev/serial/by-id/...`) therefore started before the stick was there, failed with "no such file or directory", and stayed stopped: Docker never retries a container that failed to start, whatever its restart policy. Zigbee2MQTT was down after every restart until it was started by hand.

## What changed

- **The machine waits for your USB devices before it starts any container.** At boot, lighter tells the machine which held devices are plugged in, and the machine waits until each is attached and has its `/dev/serial/by-id` name before Docker starts. A container naming one comes back with it, as it would on a Linux machine with the stick plugged in. Measured on a Home Assistant Connect ZBT-2: the stick was there 2.4 to 4.4 seconds into boot, and boot waited exactly that long.
- **Nothing waits for a device that is not coming.** Only devices plugged into the Mac are waited for, and never for more than fifteen seconds; a machine with no USB devices starts as before.

## Upgrading

```
brew upgrade lighter
lighter restart
```

Or `lighter update download` then `lighter upgrade --restart` if you installed with the install script.

## Also

- Linux remains **6.18.52** (kernel f4dfc278). The data epoch remains **1**.
