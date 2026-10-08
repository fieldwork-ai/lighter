# USB Devices: Zigbee, Z-Wave, and Serial Sticks

lighter lets you pass USB devices plugged into your Mac directly into your containers, exactly as if they were plugged into a dedicated Linux machine. Containers see standard `/dev/serial/by-id/...` device paths, so existing Zigbee2MQTT, Home Assistant, and Z-Wave JS setups migrate over unchanged.

This guide takes you from a stick in a drawer to live sensors in Home Assistant. The walkthrough below was tested end-to-end on Apple Silicon with a Home Assistant Connect ZBT-2 coordinator, a ThirdReality temperature/humidity sensor, and a smart color bulb.

## Supported Hardware

- **Supported out of the box:** Zigbee coordinators, Z-Wave dongles, Thread radios, and USB-to-serial adapters (chips including CDC-ACM, CH34x, CP210x, FTDI, and PL2303). This covers nearly every smart home coordinator on the market.
- **Recommended coordinators:** For Home Assistant and Zigbee2MQTT, choose a stick with EmberZNet or Z-Stack firmware, such as the Home Assistant Connect ZBT-2 or a Sonoff Dongle-E/P.
- **Not currently supported:** Keyboards, mice, USB mass storage drives, hubs, Bluetooth dongles, webcams, and audio interfaces. lighter automatically protects devices your Mac is actively using.

Note: Sticks using BLZ firmware (such as ThirdReality's dongle) pass through cleanly, but neither Home Assistant nor Zigbee2MQTT officially supports BLZ yet without custom forks.

---

## 1. Find Your Stick

Plug your USB device into your Mac, then list connected devices:

```bash
lighter usb list
```

Output:
```text
USB devices on this Mac:
  303a:831a  Nabu Casa ZBT_2 serial E072A1D9E0CC  full speed /dev/cu.usbmodemE072A1D9E0CC1
             not attached
```

The string `303a:831a` is the device's Vendor ID and Product ID (VID:PID), which you use to attach it.

---

## 2. Attach It

Attach the stick to lighter:

```bash
lighter usb attach 303a:831a
```

Output:
```text
303a:831a (Nabu Casa ZBT_2) is attached.
  docker run --device /dev/serial/by-id/usb-Nabu_Casa_ZBT-2_E072A1D9E0CC-if00 ...
```

The path printed on the second line is the stable Linux device name to pass into your container.

You only need to run this command once: lighter remembers attached devices in its configuration, automatically reconnecting them across VM restarts, Mac sleep/wake cycles, and physical replugs. After a restart, the machine waits for the devices the Mac is attaching before it starts any container, so a container that names one with `devices:` comes back with it.

To release the stick back to macOS at any time, run `lighter usb detach 303a:831a`.

**Helpful Tips:**
- **Machine not running yet?** `lighter usb attach` records your choice and attaches the device automatically the next time lighter starts. Once running, view all active paths with `lighter usb ls-serial`.
- **Multiple identical sticks?** Distinguish them by appending the serial number: `lighter usb attach 303a:831a:E072A1D9E0CC`.
- **"Port is open in..."?** A macOS app (like a serial terminal or firmware flasher) is currently using the device. Close that app and lighter will attach the stick automatically.

---

## 3. Run Zigbee2MQTT and Home Assistant

*Setting up Home Assistant from scratch? Start with [Home Assistant on your Mac](home-assistant.md), which puts it on your network so it discovers your devices.*

Here is a ready-to-run Docker Compose stack with Mosquitto (MQTT broker), Zigbee2MQTT, and Home Assistant. Create a new directory and save the following three files:

### `compose.yaml`

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
    depends_on:
      - mosquitto
    ports:
      - "8080:8080"
    volumes:
      - ./zigbee2mqtt:/app/data
    devices:
      # Use the path printed by `lighter usb attach`:
      - /dev/serial/by-id/usb-Nabu_Casa_ZBT-2_E072A1D9E0CC-if00:/dev/ttyACM0

  homeassistant:
    image: ghcr.io/home-assistant/home-assistant:stable
    restart: unless-stopped
    ports:
      - "8123:8123"
    volumes:
      - ./homeassistant:/config
```

### `mosquitto/mosquitto.conf`

```text
listener 1883
allow_anonymous true
```

### `zigbee2mqtt/configuration.yaml`

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

*(The `serial:` block above is configured for the Home Assistant Connect ZBT-2. For other dongles, check [Zigbee2MQTT's supported adapters guide](https://www.zigbee2mqtt.io/guide/adapters/) for their standard serial settings).*

Now start the stack:

```bash
docker compose up -d
```

- **Zigbee2MQTT Web UI:** Open [http://localhost:8080](http://localhost:8080). The logs will display `Zigbee2MQTT started!`.
- **Home Assistant Web UI:** Open [http://localhost:8123](http://localhost:8123). Complete the initial setup, then navigate to **Settings > Devices & Services > Add Integration > MQTT**, and enter `mosquitto` for the broker hostname with port `1883`.

---

## 4. Pair Your Devices

1. In the Zigbee2MQTT web interface, click **Permit join (All)**.
2. Put your Zigbee sensor or bulb into pairing mode (typically by holding the reset button for 5 seconds, or toggling a bulb off and on 5 times).
3. The device will appear in Zigbee2MQTT within seconds and automatically populate into Home Assistant via MQTT discovery.

*Prefer Home Assistant's built-in ZHA integration?* Simply omit the `zigbee2mqtt` service from `compose.yaml`, map the device directly to the `homeassistant` service, and configure ZHA on `/dev/ttyACM0`. Note that a single physical USB stick can only be managed by one coordinator service at a time.

---

## Moving From a Raspberry Pi or Home Assistant OS

Because your Zigbee network state is stored on the coordinator stick and in Zigbee2MQTT's data folder, migrating from an existing setup preserves all paired devices without re-pairing:

1. Stop Zigbee2MQTT on your old machine and copy its entire data folder (`configuration.yaml`, `coordinator_backup.json`, `database.db`, etc.) into `./zigbee2mqtt/`.
2. Ensure `serial: port:` is set to `/dev/ttyACM0` and `mqtt: server:` is set to `mqtt://mosquitto:1883`.
3. Move the USB stick to your Mac, run `lighter usb attach <vid:pid>`, and start your stack with `docker compose up -d`.

Zigbee2MQTT will resume your existing network immediately. For Home Assistant itself, create a backup on your old host and restore it during the onboarding screen on your Mac.

---

## Important Tips & Behavior

- **Always use `restart: unless-stopped`:** When macOS goes to sleep, USB power is maintained, but the host stops acknowledging packets. Zigbee coordinators will temporarily timeout and Zigbee2MQTT will exit. With `restart: unless-stopped`, Docker automatically restarts Zigbee2MQTT within one second of your Mac waking up, restoring your network seamlessly.
- **Physical replugs:** If you unplug a stick while containers are running, lighter will automatically re-attach it to the VM within one second when plugged back in. Run `docker compose up -d` to restart any containers that stopped while the device was absent.
- **Clean crash safety:** If lighter is unexpectedly stopped (`kill -9`), an independent supervisor helper process immediately returns all seized USB devices back to macOS.

---

## Troubleshooting

| What you see | How to resolve |
|---|---|
| `lighter usb list` does not show the stick | Check the cable or try another port. macOS must detect the hardware first; verify it appears in **Apple Menu > About This Mac > System Report > USB**. |
| `is not attached yet: ... is open in <app>` | A macOS application has the serial port open. Close that application and lighter will attach the stick automatically. |
| `refused` in `lighter usb list` | The device was identified as an input, storage, or hub device. Pass `--force` to attach it anyway if you are certain it is safe. |
| Container error: `no such file or directory` | Run `lighter usb ls-serial` to confirm the exact device path, and check `lighter status` to verify the device is listed as `attached`. |
| Zigbee2MQTT log: `Adapter disconnected` | The USB stick was disconnected or the Mac went to sleep. With `restart: unless-stopped`, it recovers automatically on wake. After a physical replug, run `docker compose up -d`. |

To inspect all currently attached USB devices at any time, run:
```bash
lighter status
lighter usb ls-serial
```
