# navidrome-discord-rpc

Show the track that is currently playing on your [Navidrome](https://www.navidrome.org/)
server as **Discord Rich Presence** — a "Listening to ..." status with the
track title, artist, album and a live progress bar.

```
🎵 Listening to Navidrome
   Never Gonna Give You Up
   Rick Astley — Whenever You Need Somebody
   01:12 ──────────────── 03:32
```

## How it works

The program polls Navidrome's Subsonic API (`/rest/getNowPlaying`) on a short
interval and pushes the current track to Discord over the local Discord IPC
socket. It is a single small Rust binary with no external services.

## Requirements

- A **Discord application** (free) so the status has a name and icon — see below.
- A reachable **Navidrome** instance.
- Discord running locally (the desktop client).
- Rust 1.85+ to build (edition 2024).

## Setup

### 1. Discord application (optional)

The bridge ships with a built-in Discord Application ID, so it works out of the
box. If you want the status to show under **your own** application name and icon
instead, create one:

1. Open <https://discord.com/developers/applications> → **New Application**.
2. Give it a name (this is what shows up in the status, e.g. `Navidrome`).
3. Copy the **Application ID** from *General Information* and set it as
   `DISCORD_CLIENT_ID`.
4. *(Optional)* Upload an icon under **Rich Presence → Art Assets** for a nicer
   status picture.

> The Application ID is a **public identifier** (like a bot's user ID) — it is
> not a secret. The client *secret* is never used by this program, so nothing
> sensitive is bundled or needed.

### 2. Build

```bash
cargo build --release
```

The binary lands in `target/release/navidrome-discord-rpc`.

### 3. Run

```bash
DISCORD_CLIENT_ID=123456789012345678 \
NAVIDROME_URL=http://localhost:4533 \
NAVIDROME_USER=your_user \
NAVIDROME_PASSWORD=your_password \
./target/release/navidrome-discord-rpc
```

## Configuration

All configuration is via environment variables.

| Variable             | Required | Default                 | Description                       |
|----------------------|----------|-------------------------|-----------------------------------|
| `NAVIDROME_USER`     | yes      | —                       | Navidrome username                |
| `NAVIDROME_PASSWORD` | yes      | —                       | Navidrome password                |
| `NAVIDROME_URL`      | no       | `http://localhost:4533` | Navidrome base URL                |
| `DISCORD_CLIENT_ID`  | no       | built-in application    | Discord Application ID to show as |
| `POLL_INTERVAL`      | no       | `3`                     | Poll interval, in seconds         |

## Run as a systemd user service

Create `~/.config/systemd/user/navidrome-discord-rpc.service`:

```ini
[Unit]
Description=Navidrome Discord RPC
After=network.target

[Service]
Type=simple
ExecStart=%h/navidrome-discord-rpc/target/release/navidrome-discord-rpc
Environment=NAVIDROME_URL=http://localhost:4533
Environment=NAVIDROME_USER=your_user
Environment=NAVIDROME_PASSWORD=your_password
# Optional: only if you use your own Discord application.
# Environment=DISCORD_CLIENT_ID=123456789012345678
Restart=on-failure

[Install]
WantedBy=default.target
```

Then enable and start it:

```bash
systemctl --user daemon-reload
systemctl --user enable --now navidrome-discord-rpc.service
```

> **Tip:** for anything more than a personal machine, keep the password out of
> the unit file — put the variables in a `~/.config/navidrome-discord-rpc.env`
> file (`chmod 600`) and reference it with `EnvironmentFile=` instead.

## Behaviour

- **Listening** activity type with a progress bar that Discord advances itself.
  The presence is re-sent on every poll, so the bar stays in sync with
  Navidrome (seeks and buffering show up within one poll interval).
- **Paused** tracks are marked `⏸ Paused` and their progress slider *freezes* at
  the paused position (it is not removed), so a pause stays visible.
- Between tracks Navidrome briefly reports nothing; the presence is held for a
  short grace period so it doesn't flicker.
- When playback really stops, the presence is cleared after that grace period.
- If Discord restarts, the client reconnects automatically.
- The log only prints when the presence actually changes.

> **Responsiveness:** pause/seek are detected within `POLL_INTERVAL` seconds
> (default `3`). Navidrome has no push API, so this is the cost of polling;
> lower `POLL_INTERVAL` (e.g. `1`) for a snappier status at the cost of more
> local API requests.

## License

AGPL-3.0-or-later — see [LICENSE](LICENSE).