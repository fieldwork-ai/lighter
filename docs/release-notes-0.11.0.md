# lighter 0.11.0

USB devices plugged into your Mac can now be used by containers, as if they were plugged into a Linux box. The first target is the home lab: a Zigbee or Z-Wave coordinator for Home Assistant, Zigbee2MQTT or Z-Wave JS, under the same stable names Linux gives it, so an existing configuration moves over unchanged.

## Using it

```
lighter usb list                 # the Mac's USB devices, and which the guest has
lighter usb attach 303a:831a     # vendor:product, from the list
lighter usb ls-serial            # the guest's /dev/serial/by-id names
```

`attach` prints the path to hand a container:

```yaml
services:
  zigbee2mqtt:
    image: koenkk/zigbee2mqtt
    devices:
      - /dev/serial/by-id/usb-Nabu_Casa_ZBT-2_E072A1D9E0CC-if00:/dev/ttyACM0
```

- **It stays attached:** across machine restarts, and when it is unplugged and plugged back in, until `lighter usb detach`, which gives it back to macOS at once.
- **Two of the same:** add the serial number, `lighter usb attach 303a:831a:E072A1D9E0CC`.
- **`lighter status`** shows each attached device and where it stands.
- **No root, no helper app, no entitlement:** lighter asks macOS's driver to let go of the device, which Apple's own drivers do, and gives it back when detached.

## What is refused, and why

`attach` refuses these unless given `--force`:
- **An input device** (keyboard, mouse or similar): taken from macOS, it stops working on the Mac.
- **A storage device:** macOS may have it mounted.
- **A device whose serial port a Mac program has open:** the refusal names the program. The device stays in the configuration and is attached as soon as the program lets go.

Also refused:
- **USB hubs:** attach the devices behind them instead.
- **Bluetooth adapters:** not supported yet. Home Assistant's Bluetooth needs BlueZ running in the guest, which is a release of its own.

Devices that need isochronous transfers (webcams, audio interfaces) are not supported yet.

## If lighter is killed

A device lighter holds is out of macOS's reach until it is given back. If the machine's process dies holding devices, even with `kill -9`, a small keeper process gives them back to macOS within a second. The next start repairs anything left over, for instance after a power cut.

## Speed

Measured through a container on an M5, against the same client run natively on the Mac:

| | through lighter | native |
|---|---|---|
| Home Assistant Connect ZBT-2, firmware probe (EZSP at 460800 baud) | 3.02–3.11 s | 2.98–2.99 s |
| ThirdReality Zigbee dongle, BLZ request at 2 Mbaud | 4.5 ms | 4.5 ms |

The difference on the ZBT-2 is `docker exec`'s own start-up.

## How

The Mac serves each device over USB/IP, the protocol Linux's `vhci-hcd` speaks, on a vsock stream of its own. The guest's kernel takes the device as plugged into a port, and Linux's own drivers bind: `cdc_acm` for CDC devices like the ZBT-2, and the CH34x, CP210x, FTDI and PL2303 USB-to-serial drivers. The guest's agent keeps `/dev/serial/by-id` from the kernel's events, named exactly as udev names them.

- **The Mac's side** is asynchronous throughout: one thread for every device, each transfer an asynchronous IOUSBHost request.
- **The protocol** follows Linux's own USB/IP server, including its unlink rules.
- **The replies waiting for the guest are bounded:** past the bound, new transfers wait, while unlinks are still acted on.

`docs/architecture.md`, "USB", has the details.

## Checked

Gate m15-usb, part one, runs unattended with a ZBT-2 and a ThirdReality dongle plugged in, which between them cover both of macOS's driver models and both of Linux's. It checks:
- both attach from the configuration at start, under udev's names, and macOS's own ports go away;
- each radio answers its firmware query through a container;
- detach gives the device back to macOS, and attach names its path;
- detach and attach back to back, three times over, leave the radio answering;
- a device the guest drops is attached again on its own;
- the guest's agent can restart under attached devices;
- a port a Mac program holds is refused, naming the program, then attached once it is let go;
- a machine stopped and started again attaches its devices again;
- a machine killed with `kill -9` gives them back to macOS.

Part two runs with a person at the Mac: pairing a Zigbee sensor and bulb, unplugging a stick by hand, and sleeping the Mac.

## Also

- The guest kernel gains USB, USB/IP's host controller and the serial drivers above, and nothing for real controllers. The image is 600 KB larger.
- Linux remains **6.18.52**; the data epoch remains **1**.
