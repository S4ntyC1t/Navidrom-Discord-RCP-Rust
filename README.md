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

| Variable               | Required | Default                 | Description                                            |
|------------------------|----------|-------------------------|--------------------------------------------------------|
| `NAVIDROME_USER`       | yes      | —                       | Navidrome username                                     |
| `NAVIDROME_PASSWORD`   | yes      | —                       | Navidrome password                                     |
| `NAVIDROME_URL`        | no       | `http://localhost:4533` | Navidrome base URL                                     |
| `NAVIDROME_PUBLIC_URL` | no       | —                       | Public origin serving the artwork — required for covers |
| `COVER_MODE`           | no       | `off`                   | Where cover art comes from: `off`, `navidrome`, `server` |
| `DISCORD_CLIENT_ID`    | no       | built-in application    | Discord Application ID to show as                      |
| `POLL_INTERVAL`        | no       | `1`                     | Poll interval, in seconds                              |

### Credentials

The password is **never put on the wire**. Requests carry the Subsonic token
pair instead — `t=md5(password + salt)` with a fresh random salt per request —
so nothing between this bridge and Navidrome, and no line in the journal, ever
sees the password itself.

This is why `NAVIDROME_PASSWORD` still exists as a variable: it is the secret
the token is derived from, held in memory and nowhere else.

### Cover art

Cover art is **opt-in**, and the reason is structural: Discord downloads
presence images **from its own servers**, so it cannot reach your `localhost`.
Some public endpoint is therefore unavoidable — and most setups do not have
one. Rather than pretend otherwise, the bridge ships with covers off and says
so once at startup.

`COVER_MODE` names where the images come from:

| Mode | Images served by | Needs |
|------|------------------|-------|
| `off` *(default)* | nobody — plain status | nothing at all |
| `navidrome` | Navidrome itself | Navidrome reachable from the internet |
| `server` | the bundled `serve-covers` process | that process, behind a tunnel or reverse proxy |

```ini
# Navidrome is already exposed (many people do this for the mobile app):
Environment=COVER_MODE=navidrome
Environment=NAVIDROME_PUBLIC_URL=https://music.example.com
```

Both real modes also want `NAVIDROME_PUBLIC_URL` — the origin Discord should
fetch from. The bridge rewrites the cover URLs it is handed to that origin,
leaving the signed path intact.

#### Serving covers without exposing Navidrome

`COVER_MODE=server` pairs the bridge with a second process, `serve-covers`,
which serves artwork and nothing else. It is worth running when you want
covers but would rather not publish Navidrome: putting a login form and an API
on the internet alongside a JPEG is a bad trade.

It binds loopback only and holds no credentials. The `/share/img/<JWT>` URLs it
proxies are signed but credential-free, so compromising the process cannot
reach your account — and it answers `404` to anything that is not a
well-formed artwork path, before any request is made upstream. `/login`,
`/rest/…` and path traversal are all refused.

Run it as a second user service:

```ini
# ~/.config/systemd/user/navidrome-discord-covers.service
[Unit]
Description=Navidrome cover art server
After=network.target

[Service]
Type=simple
ExecStart=%h/navidrome-discord-rpc/target/release/navidrome-discord-rpc serve-covers
Environment=COVERS_LISTEN=127.0.0.1:4534
Environment=NAVIDROME_URL=http://localhost:4533
Restart=on-failure

[Install]
WantedBy=default.target
```

Then publish that port through any tunnel or reverse proxy — TLS terminates in
front of it, and only this process is ever reachable, never Navidrome. With
[Tailscale Funnel](https://tailscale.com/kb/1223/funnel):

```bash
tailscale funnel --https=8443 --bg 127.0.0.1:4534
```

```ini
Environment=COVER_MODE=server
Environment=NAVIDROME_PUBLIC_URL=https://<your-node>.<your-tailnet>.ts.net:8443
```

> **Choose the port deliberately.** Tailscale decides whether a hostname is
> public or tailnet-only **per port, not per path** — so funnelning a port that
> an existing `serve` rule already occupies will publish whatever that rule was
> pointing at, not just the covers. If `tailscale serve status` shows a rule on
> `:443`, use `8443` or another port and leave `443` alone.

Any reverse proxy works equally well. `serve-covers` only knows how to serve
images; the bridge only needs an origin Discord can reach.

**The startup line tells you which of three situations you are in**, and they
are deliberately worded differently:

```
Cover art: off — set COVER_MODE=navidrome (or server) …     # nothing configured yet
Cover art: from Navidrome at https://…                      # configured and workable
Warning: COVER_MODE=server but NAVIDROME_PUBLIC_URL …       # asked for, will not appear
```

The first is a hint, not a failure — a first run with no public endpoint has
nothing wrong with it. The last two are the only things worth acting on: a
mode selected without an origin, or an origin set while the mode is still
`off`. An unknown `COVER_MODE` is rejected outright at startup with exit code
`2` rather than falling back to `off`, because a typo that silently disables
the feature is exactly what the variable exists to prevent.

`server` mode never falls back to Navidrome's own URLs. That mode exists
*because* Navidrome is private, so handing Discord an address already known to
be unreachable would just produce a blank square with no explanation.

> **Keep the credentials out of those URLs.** Navidrome's cover endpoints
> (`/rest/getCoverArt`) put the username and password into the query string,
> which would hand your account to Discord along with the artwork. This bridge
> uses `/rest/getAlbumInfo` instead, which returns `/share/img/<JWT>` — a
> signed, **credential-free** URL. Do not "fix" a missing cover by pointing it
> back at `getCoverArt`.

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
# Optional: cover art. Pick a mode, then the origin Discord should fetch from.
# Environment=COVER_MODE=navidrome
# Environment=NAVIDROME_PUBLIC_URL=https://music.example.com
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
  the bridge. Everything interpolated into a query — username, salt, track id —
  is percent-encoded, so values containing `&`, spaces or non-ASCII work.
- **Album art** is the presence's large image, with the album name on hover.
  The URL comes from `/rest/getAlbumInfo`, which serves a signed
  `/share/img/<JWT>` with **no credentials embedded** — Navidrome's
  `getCoverArt` would otherwise put your username and password into a URL that
  is handed to a third party. Artwork is resolved once per track and cached, so
  pausing costs no extra request.
- A position reported past the end of a track is clamped, so the bar never
  renders an inverted window.
- Two links sit under the status: one to [Navidrome](https://www.navidrome.org/)
  itself — a small thank-you to the server this bridge exists to mirror — and
  one to this repository. They are static, so they survive pausing.

> **Responsiveness:** pause/seek are detected within `POLL_INTERVAL` seconds
> (default `1`). Navidrome has no push API, so this is the cost of polling;
> raise `POLL_INTERVAL` if you'd rather make fewer local API requests at the
> cost of a slower status.

## Troubleshooting

**You can't see the buttons.** That is Discord's rule, not a bug here: buttons
are [only rendered to *other* users](https://docs.discord.com/developers/discord-social-sdk/development-guides/setting-rich-presence)
— the account that set the activity never sees them on itself. To check they
work, look at your profile from a second account, or ask a friend.

**No album art.** Four things must line up, and the startup log says which is
missing:

1. `COVER_MODE` must be `navidrome` or `server`. It defaults to `off`, and the
   bridge prints `Cover art: off — set COVER_MODE=…` when nothing has been
   chosen.
2. `NAVIDROME_PUBLIC_URL` must be an origin Discord can reach from the
   internet. Missing it produces a `Warning:` at startup and no image —
   deliberately, since a `localhost` URL would just fail to load. If Navidrome
   already has a public `BaseUrl`, `navidrome` mode can work without it.
3. It must be **https**. Discord may refuse a plain `http://` image, and the
   symptom is a silently missing cover.
4. The album must have artwork at all. Navidrome reports this as an empty
   `albumInfo`, and the bridge skips it rather than sending a broken frame.

In `server` mode also confirm the cover server is up and actually published —
it must be reachable from the internet through whatever tunnel or reverse proxy
you put in front of it:

```bash
systemctl --user status navidrome-discord-covers
curl -I https://your-tunnel-origin/share/img/<token>
```

If art is configured but a track reports `Cover art lookup failed …`, the
request itself is failing — check that `NAVIDROME_URL` is right and the account
may read the album.

> **Do not "fix" this by pointing at `/rest/getCoverArt`.** That endpoint puts
> your username and password in the query string, and the URL is then handed to
> Discord. The whole reason this bridge uses `/rest/getAlbumInfo` is that it
> returns a signed, credential-free URL.

**The status vanishes, flickers, or shows someone else's.** Discord keeps a
single activity slot and the *last* writer wins, so any other application with
its own Rich Presence will overwrite this one whenever it updates. Games that
set their presence once at launch are harmless; chatty clients are not. Check
what else is connected:

```bash
ss -xap | grep discord-ipc
```

Then close that application, or disable its Discord integration. This bridge
re-sends its activity on every poll, so it reclaims the slot within a second —
but it cannot stop another client from taking it again.

## Disclosure

This project was written with the help of an AI coding agent —
**[OpenCode](https://opencode.ai)**, running the `mimo-v2.6-flash-free`
model. The code, the tests, the bug fixes and the commit messages all came
out of a conversation with it.

Everything described under [Behaviour](#behaviour) was verified against a
live Navidrome server and the Discord desktop client — but only on **Linux**.
macOS, BSD and Windows have never been run (see [Platform](#platform)); the
Windows case is reasoned about and explicitly rejected, not tested.

## License

AGPL-3.0-or-later — see [LICENSE](LICENSE).
