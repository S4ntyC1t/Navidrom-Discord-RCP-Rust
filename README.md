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

## Platform

**Unix-like systems only (Linux, macOS, BSD).** The bridge talks to Discord
over a Unix domain socket (`$XDG_RUNTIME_DIR/discord-ipc-N`). Windows has no
Unix sockets — it uses named pipes (`\\?\pipe\discord-ipc-N`) instead — so the
build fails there with an explicit `compile_error!` rather than a confusing
type error.

Nothing else about the program is Unix-specific: Navidrome is reached over
HTTP, and the config is plain environment variables. Windows support would
mean adding a named-pipe transport alongside the existing one; contributions
welcome.

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

`--help` prints the environment variables it reads, `--version` prints the
build version. Both exit before touching the network.

## Configuration

All configuration is via environment variables.

| Variable             | Required | Default                 | Description                       |
|----------------------|----------|-------------------------|-----------------------------------|
| `NAVIDROME_USER`     | yes      | —                       | Navidrome username                |
| `NAVIDROME_PASSWORD` | yes      | —                       | Navidrome password                |
| `NAVIDROME_URL`      | no       | `http://localhost:4533` | Navidrome base URL                |
| `DISCORD_CLIENT_ID`  | no       | built-in application    | Discord Application ID to show as |
| `POLL_INTERVAL`      | no       | `1`                     | Poll interval, in seconds         |

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
- **Paused** tracks are marked `⏸ Paused` and carry **no timestamps**, so the
  progress bar stops moving (Discord has no native paused state — it always
  renders the time as "now minus start" — so omitting the timestamps is the only
  way to keep it still). The pause stays visible via the marker.
- Every reply Discord sends is read back and matched to its request, and the
  activity Discord echoes is compared against the one that was sent. If Discord
  ever stored timestamps we deliberately omitted (i.e. kept a stale bar), the
  bridge logs it instead of silently pretending the update landed.
- Between tracks Navidrome briefly reports nothing; the presence is held for a
  short grace period so it doesn't flicker.
- When playback really stops, the presence is cleared after that grace period.
- If Discord restarts, the client reconnects automatically.
- The log only prints when the presence actually changes, and a repeated error
  (Navidrome down, Discord closed) is logged once rather than once per poll.
- Each request to Navidrome has a 10s timeout, so a hung server cannot freeze
  the bridge. Credentials are percent-encoded, so passwords containing `&`,
  spaces or non-ASCII work.
- A position reported past the end of a track is clamped, so the bar never
  renders an inverted window.

> **Responsiveness:** pause/seek are detected within `POLL_INTERVAL` seconds
> (default `1`). Navidrome has no push API, so this is the cost of polling;
> raise `POLL_INTERVAL` if you'd rather make fewer local API requests at the
> cost of a slower status.

## License

AGPL-3.0-or-later — see [LICENSE](LICENSE).
