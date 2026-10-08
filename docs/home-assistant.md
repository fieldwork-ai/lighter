# Home Assistant on your Mac

Run Home Assistant on the Mac you already have. With lighter it gets its own address on your network, just like a dedicated box, so it finds your lights, speakers, printers and NAS by itself. Setup takes a few minutes.

Everything below was run exactly as written on an 8 GB M1 Mac on Wi-Fi, with Home Assistant 2026.10 and lighter 0.12.5.

## 1. Put lighter on your network

```bash
lighter config --lan on
lighter restart
lighter status
```

Look for the `lan` line. That's Home Assistant's address on your network:

```
  lan        192.168.50.162/24 on Wi-Fi en0
```

That's all it takes: no `sudo`, no extra software. The address stays the same every time lighter starts.

## 2. Start Home Assistant

```bash
mkdir -p ~/homeassistant && cd ~/homeassistant
```

Save this as `~/homeassistant/compose.yaml`, with your own time zone:

```yaml
services:
  homeassistant:
    image: ghcr.io/home-assistant/home-assistant:stable
    container_name: homeassistant
    # Shares lighter's address on your network, so discovery works.
    network_mode: host
    restart: unless-stopped
    environment:
      TZ: Europe/London
    volumes:
      - ./config:/config
```

```bash
docker compose up -d
```

## 3. Open it

Give it a minute on first start, then open **http://localhost:8123** on your Mac, or **http://192.168.50.162:8123** (your `lan` address) from your phone. Create your account and you're in.

## 4. Watch your devices appear

Open **Settings → Devices & services**. On our test network, Home Assistant found all of these by itself within minutes:

- a Philips Hue bridge
- a Sonos speaker and Google Cast devices (added automatically)
- a WiiM amplifier
- a Brother printer
- a Synology NAS
- the router

Click **Add** on any of them and you're done.

## 5. Keep it running

- **Start at login:** `lighter install`. Home Assistant comes back by itself whenever lighter starts.
- **Update:** `cd ~/homeassistant && docker compose pull && docker compose up -d`.
- **Back up:** use **Settings → System → Backups**. Everything Home Assistant knows lives in `~/homeassistant/config`, a normal folder on your Mac.

## Add more

- **Zigbee and Z-Wave:** plug your stick into the Mac and follow the [USB guide](usb.md). With Home Assistant on host networking, add `ports: ["1883:1883"]` to Mosquitto and use `localhost` as the MQTT broker.
- **Voice on your Mac's GPU:** [`examples/whisper-metal`](../examples/whisper-metal) is speech-to-text for Assist that turns 3.5 s of speech into text in 0.5 s. Add the Wyoming integration at `localhost`, port `10300`.
- **Cameras:** Frigate runs its detector on the Mac's Neural Engine. See [Frigate](../README.md#home-lab-frigate-home-assistant-pi-hole).

## Coming from Home Assistant OS?

Make a full backup on your old system, then pick **Restore from backup** on the welcome screen. Your devices, automations and history come with it. Add-ons are a Home Assistant OS feature, so run what they ran as containers instead: Mosquitto and Zigbee2MQTT are in the [USB guide](usb.md).

## Troubleshooting

| What you see | What to do |
|---|---|
| `lighter status` says `waiting for an address` | Some routers won't lease one on Wi-Fi. Pick a free address outside your router's DHCP range: `lighter config --lan-address 192.168.1.240`, then `lighter restart`. |
| Nothing shows up as discovered | Check the `lan` line in `lighter status`, and that `compose.yaml` has `network_mode: host`. |
| Your phone can't open it | Use the `lan` address from `lighter status`, not your Mac's own. |
| Port 8123 is already in use | Something else on your Mac is using it. Quit it, then `docker compose up -d` again. |
