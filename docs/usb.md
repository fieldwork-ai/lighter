# USB devices: Zigbee, Z-Wave and serial sticks

lighter can hand a USB device plugged into your Mac to your containers, as if it were plugged into a Linux box. The container sees the same `/dev/serial/by-id/…` name Linux would give it, so an existing Zigbee2MQTT, Home Assistant or Z-Wave JS setup moves over unchanged.

This guide takes you from a stick in a drawer to sensors in Home Assistant. The steps below were run as written on an M5 MacBook Pro, with a Home Assistant Connect ZBT-2, a ThirdReality temperature sensor and a ThirdReality colour bulb.

## What works

- **Serial sticks:** Zigbee coordinators, Z-Wave sticks, Thread radios and USB-to-serial adapters (CDC-ACM, CH34x, CP210x, FTDI and PL2303 chips). These cover nearly every home automation stick.
- **Not yet:** keyboards, mice and other input devices, USB storage, hubs, Bluetooth adapters, webcams and audio interfaces. lighter refuses the first four so it never takes a device your Mac is using, and says why.

For Home Assistant and Zigbee2MQTT, choose a stick with EmberZNet or Z-Stack firmware, such as the ZBT-2 or a SONOFF dongle. Sticks that speak BLZ, such as ThirdReality's own dongle, pass through fine, but neither Home Assistant nor Zigbee2MQTT supports BLZ yet.

## 1. Find your stick

Plug it in, then:

```
$ lighter usb list
USB devices on this Mac:
  303a:831a  Nabu Casa ZBT_2 serial E072A1D9E0CC  full speed /dev/cu.usbmodemE072A1D9E0CC1
             not attached
```

`303a:831a` is the stick's vendor and product ID, which is how you name it to lighter.

## 2. Attach it

```
$ lighter usb attach 303a:831a
303a:831a (Nabu Casa ZBT_2) is attached.
  docker run --device /dev/serial/by-id/usb-Nabu_Casa_ZBT-2_E072A1D9E0CC-if00 …
```

The path it prints is the one to give your container. You only do this once: the stick stays attached across restarts and re-plugs until you run `lighter usb detach 303a:831a`, which gives it back to macOS straight away.

- **Machine not running?** `attach` says the stick will be attached when it starts. Once it is up, `lighter usb ls-serial` shows the path.
- **Two identical sticks?** Add the serial number: `lighter usb attach 303a:831a:E072A1D9E0CC`.
- **"is open in …"?** A Mac app has the stick's port open, often a leftover flashing tool or serial monitor. Quit it and the stick attaches by itself.

## 3. Run Zigbee2MQTT and Home Assistant

A folder with three files runs the usual stack: an MQTT broker, Zigbee2MQTT and Home Assistant.

**`compose.yaml`**

```yaml
services:
  mosquitto:
    image: eclipse-mosquitto:2
    restart: unless-stopped
    volumes:
      - ./mosquitto/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro

  zigbee2mqtt:
    image: koenkk/zigbee2mqtt
    restart: unless-stopped
    depends_on: [mosquitto]
    ports:
      - "8080:8080"
    volumes:
      - ./zigbee2mqtt:/app/data
    devices:
      # The path `lighter usb attach` printed, mapped to the name in the config below.
      - /dev/serial/by-id/usb-Nabu_Casa_ZBT-2_E072A1D9E0CC-if00:/dev/ttyACM0

  homeassistant:
    image: ghcr.io/home-assistant/home-assistant:stable
    restart: unless-stopped
    ports:
      - "8123:8123"
    volumes:
      - ./homeassistant:/config
```

**`mosquitto/mosquitto.conf`**

```
listener 1883
allow_anonymous true
```

**`zigbee2mqtt/configuration.yaml`**

```yaml
homeassistant:
  enabled: true
mqtt:
  server: mqtt://mosquitto:1883
serial:
  port: /dev/ttyACM0
  adapter: ember
  baudrate: 460800
  rtscts: true
frontend:
  enabled: true
  port: 8080
advanced:
  network_key: GENERATE
  pan_id: GENERATE
  ext_pan_id: GENERATE
```

The `serial:` settings are the ZBT-2's. For another stick, use the settings from [Zigbee2MQTT's adapter list](https://www.zigbee2mqtt.io/guide/adapters/); they are the same as on any Linux host.

Then:

```
docker compose up -d
```

- **Zigbee2MQTT** is at http://localhost:8080. Its log should end with `Zigbee2MQTT started!`.
- **Home Assistant** is at http://localhost:8123. Create your account, then go to Settings › Devices & services › Add integration › MQTT, and enter `mosquitto` as the broker, port `1883`.

## 4. Pair devices

In Zigbee2MQTT, click **Permit join (All)**, then put each device in pairing mode. Most devices pair from a reset: hold a button for about five seconds, or for bulbs, switch them off and on five times. Each one appears in Zigbee2MQTT within seconds, and in Home Assistant a moment later, through MQTT.

Prefer ZHA, Home Assistant's built-in Zigbee support, to Zigbee2MQTT? Leave out the `zigbee2mqtt` service, give Home Assistant the `devices:` line instead, and add the Zigbee Home Automation integration with the port `/dev/ttyACM0`. Use one or the other: a stick serves only one of them at a time. (This ZHA route is Home Assistant's standard setup, but unlike the rest of this guide it has not yet been run on lighter with a ZBT-2.)

## Moving from a Home Assistant VM or a Raspberry Pi

Your Zigbee network lives on the stick and in Zigbee2MQTT's data folder, so moving keeps every paired device:

1. Stop Zigbee2MQTT on the old machine and copy its whole data folder (`configuration.yaml`, `coordinator_backup.json`, `database.db` and the rest) into `./zigbee2mqtt`.
2. Change `serial: port:` to `/dev/ttyACM0`, and `mqtt: server:` to `mqtt://mosquitto:1883`.
3. Move the stick to the Mac, attach it, and `docker compose up -d`.

Zigbee2MQTT resumes the same network, and the devices do not need pairing again. For Home Assistant itself, make a backup on the old machine and restore it during onboarding on the new one. One difference if you are coming from Home Assistant OS: containers have no add-ons, which is why Mosquitto and Zigbee2MQTT are services of their own here.

## Things worth knowing

- **Keep `restart: unless-stopped`.** When the Mac sleeps, a coordinator such as the ZBT-2 stays powered, stops hearing from its host and gives up, and Zigbee2MQTT exits. With the restart policy it is back on the same network within a second of the Mac waking. This happens without lighter too: native Zigbee2MQTT on a Mac does the same.
- **After unplugging a stick, start its container again** once the stick is back: `docker compose up -d`. lighter re-attaches the stick by itself within a second, but Docker's own restart runs while the stick is missing, fails, and gives up, as it does on any Linux host.
- **Sleep and restarts are safe.** The stick stays attached through the Mac sleeping, `lighter stop` and `lighter start`. If lighter is killed, a small helper process gives the stick back to macOS within a second.

## Troubleshooting

| You see | Do this |
|---|---|
| `lighter usb list` doesn't show the stick | Try another port or cable. macOS must see it first: it should appear in System Information › USB. |
| `is not attached yet: … is open in <app>` | Quit that app. The stick attaches by itself once the port is free. |
| `refused` in `lighter usb list` | It looks like an input, storage or hub device. `--force` attaches it anyway, and macOS loses it while attached. |
| Container fails with `no such file or directory` for the device | Check `lighter usb ls-serial` for the exact path, and that `lighter status` shows the stick `attached`. |
| Zigbee2MQTT: `Adapter disconnected` | The stick went away: unplugged, or the Mac slept. With `restart: unless-stopped` it recovers by itself; after a re-plug, `docker compose up -d`. |

`lighter status` lists every attached device and where it stands. `lighter usb ls-serial` lists the names containers can use.
