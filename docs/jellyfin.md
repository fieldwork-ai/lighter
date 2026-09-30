# Jellyfin on a Mac with lighter

Jellyfin is a free media server: it serves your films and shows to the Jellyfin apps on phones, TVs and browsers. On lighter it runs in a container like it would on a Linux server. The difference is that it can use your Mac's media engine to transcode.

Everything below was run as written on an M5 Pro MacBook Pro, with Jellyfin 12.1 and lighter 0.11.1.

## 1. A folder for Jellyfin

Make a folder, with a `media` folder inside for your films, organised the way Jellyfin likes them (`media/movies/Film Name (Year)/Film Name (Year).mkv`):

```
mkdir -p ~/jellyfin/media/movies
cd ~/jellyfin
```

Then save this as `~/jellyfin/compose.yaml`, putting your Mac's address in `JELLYFIN_PublishedServerUrl` (`ipconfig getifaddr en0` prints it):

```yaml
services:
  jellyfin:
    image: jellyfin/jellyfin
    container_name: jellyfin
    # The server's name in Jellyfin's apps.
    hostname: jellyfin
    restart: unless-stopped
    ports:
      - "8096:8096"
      # Lets Jellyfin apps on your network find the server.
      - "7359:7359/udp"
    volumes:
      - ./config:/config
      - ./cache:/cache
      - ./media:/media:ro
    environment:
      # The address apps should use: your Mac's, not the container's.
      JELLYFIN_PublishedServerUrl: http://192.168.1.20:8096
    devices:
      # The Mac's media engine, for hardware transcoding.
      - lighter.sh/video=all
```

Start it:

```
docker compose up -d
```

## 2. Set it up

Open http://localhost:8096 and follow Jellyfin's setup: pick a language, create your account, and add a library of type **Movies** with the folder **`/media/movies`**. Jellyfin scans it and fetches artwork.

**Media somewhere else?** Change `./media` in `compose.yaml` to the folder's path on your Mac. Anything in your home folder works as it is.

**Media on an external drive?** lighter shares only your home folder with containers, so add the drive to `"shares"` in `~/.lighter/config.json`, then run `lighter restart`:

```json
{
  "shares": ["/Users/you", "/Volumes/Media"]
}
```

Then use `/Volumes/Media` in `compose.yaml` as you would any folder. **The drive must be connected when lighter starts.** At the moment, a missing shared folder stops lighter from starting at all, not just Jellyfin. If you disconnect the drive for good, take it out of `"shares"`.

## 3. Turn on hardware transcoding

In Jellyfin, go to **Dashboard › Playback › Transcoding**, set **Hardware acceleration** to **Video4Linux2 (V4L2)**, leave **Enable hardware encoding** ticked, and save.

Jellyfin now encodes transcodes on the Mac's media engine. To check, play something at a lower quality than the original: its log under **Dashboard › Logs** (`FFmpeg.Transcode-…`) shows `h264_v4l2m2m`. A line saying `Failed to set header mode` is harmless: the video is fine.

What to expect, measured on the M5 Pro:

| Transcode | Software | Media engine |
|---|---|---|
| 1080p H.264 film to 720p for a phone | 17 s of CPU per minute of film | **10.5 s (38% less)** |
| 4K HEVC 10-bit film to 1080p | 22.7 s of CPU per 30 s | **19.3 s (15% less)** |

Both run many times faster than real time either way. The media engine's gain is CPU the Mac gets back, which matters most with several streams, or when the Mac is doing something else. It helps 4K HEVC less because, with V4L2, Jellyfin encodes on the media engine but still decodes in software.

Most apps play most files as they are, without any transcoding. Transcoding happens when a device can't play the format, or when you choose a lower quality.

## 4. Watch

On a phone, TV or another computer on the same network, open the Jellyfin app: it should find **jellyfin** by itself. If it doesn't, enter the address from `JELLYFIN_PublishedServerUrl`, such as `http://192.168.1.20:8096`.

## Things worth knowing

- **A sleeping Mac serves nothing.** For a Mac that's always on as a media server, set it not to sleep in System Settings › Energy (or Battery on a laptop).
- **Start at login:** `lighter install` starts lighter when you log in, and `restart: unless-stopped` brings Jellyfin back with it.
- **Updating Jellyfin:** `docker compose pull && docker compose up -d`. Your settings, users and watch history live in `./config`.

## Troubleshooting

| You see | Do this |
|---|---|
| The app doesn't find the server | Enter `http://<your Mac's address>:8096` by hand. Check that `7359:7359/udp` is in `compose.yaml`. |
| The app finds it, then can't connect | `JELLYFIN_PublishedServerUrl` has the wrong address: set it to your Mac's current one and `docker compose up -d`. |
| The library is empty | Check that the folder in `compose.yaml` is right, and that the library points at `/media/movies`. |
| `lighter start` fails after adding a drive | The drive isn't connected: connect it, or take it out of `"shares"`. |
| Transcoding doesn't use `h264_v4l2m2m` | Check that `lighter.sh/video=all` is under `devices:`, and that Hardware acceleration says Video4Linux2 (V4L2). |
