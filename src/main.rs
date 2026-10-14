//! Navidrome -> Discord Rich Presence bridge.
//!
//! Polls Navidrome's Subsonic API for the track that is currently playing and
//! mirrors it into Discord Rich Presence over the local Discord IPC socket.
//!
//! Configuration is provided through environment variables:
//!
//! | Variable               | Required | Default                 |
//! |------------------------|----------|-------------------------|
//! | `NAVIDROME_USER`       | yes      | —                       |
//! | `NAVIDROME_PASSWORD`   | yes      | —                       |
//! | `NAVIDROME_URL`        | no       | `http://localhost:4533` |
//! | `NAVIDROME_PUBLIC_URL` | no       | — (no cover art)        |
//! | `COVER_MODE`           | no       | `off`                   |
//! | `DISCORD_CLIENT_ID`    | no       | built-in application    |
//! | `POLL_INTERVAL`        | no       | `1` (second)            |
//!
//! Cover art is opt-in: `COVER_MODE` names where the images come from
//! (`navidrome` — Navidrome itself; `server` — the bundled `serve-covers`
//! process), and both need `NAVIDROME_PUBLIC_URL` to be something Discord can
//! reach. With no mode selected there are no covers, and the bridge says so
//! once at startup rather than leaving a blank square to be puzzling over.
//!
//! Authentication uses the Subsonic token scheme (`t`/`s`), never `p=`: the
//! password never leaves this process, so it can never end up in a URL, in the
//! journal, or in whatever sits between us and Navidrome.

#[cfg(not(unix))]
compile_error!(
    "navidrome-discord-rpc supports Unix-like systems only (Linux, macOS, BSD): \
     it talks to Discord over a Unix domain socket, and Windows does not have \
     those — it uses named pipes instead, which this transport does not implement \
     yet. See the Platform section of README.md."
);

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use discord_rich_presence::activity::{
    Activity, ActivityType, Assets, Button, StatusDisplayType, Timestamps,
};
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use serde_json::Value;

/// Discord Application ID bundled with the project. It is a public identifier
/// (not a secret — there is no client secret involved), so it is safe to ship
/// and lets the bridge work out of the box. Override it with `DISCORD_CLIENT_ID`
/// if you want the status to show under your own application name/icon.
const DEFAULT_DISCORD_CLIENT_ID: &str = "1559471162419847198";

/// Link to the Navidrome project itself.
///
/// Shown as a button under the status: a small thank-you to the server this
/// whole bridge exists to mirror. Both URLs must be `https` — Discord rejects
/// anything else.
const NAVIDROME_LINK: &str = "https://www.navidrome.org/";

/// Link to this bridge's source.
const REPOSITORY_LINK: &str = "https://github.com/S4ntyC1t/navidrome-discord-rpc";

const USAGE: &str = "\
Required environment variables:
  NAVIDROME_USER      Your Navidrome username
  NAVIDROME_PASSWORD  Your Navidrome password

Optional:
  NAVIDROME_URL        Navidrome base URL       (default http://localhost:4533)
  NAVIDROME_PUBLIC_URL Public origin serving the artwork, e.g.
                       https://music.example.com. Required for cover art:
                       Discord fetches images from its own servers, so a
                       localhost URL is unreachable for it.
  COVER_MODE           Where cover art comes from (default off):
                         off       no cover art at all
                         navidrome Navidrome serves it — must be public
                         server    `serve-covers` serves it behind a tunnel
  DISCORD_CLIENT_ID    Override the built-in Discord Application ID
  POLL_INTERVAL        Poll interval in seconds (default 1)

Cover server (`navidrome-discord-rpc serve-covers`) reads only:
  NAVIDROME_URL        Navidrome base URL       (default http://localhost:4533)
  COVERS_LISTEN        Address to bind          (default 127.0.0.1:4534)
  It needs no credentials: the image URLs it serves are signed and
  credential-free by design.";

/// Where cover art comes from.
///
/// Selected explicitly rather than inferred, so that "no covers" is a setting
/// the operator made instead of a feature that quietly failed. The default
/// needs no infrastructure whatsoever, which is the honest common case: Discord
/// downloads presence images from its own servers, so *some* public endpoint
/// is unavoidable — and most people have not set one up yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverMode {
    /// No cover art. The default, and a fully supported way to run.
    Off,
    /// Navidrome serves the artwork itself — workable only when Navidrome is
    /// publicly reachable, whether through its own `BaseUrl` or through an
    /// explicit `NAVIDROME_PUBLIC_URL`.
    Navidrome,
    /// The bundled `serve-covers` process serves the artwork behind a tunnel
    /// or reverse proxy. This mode exists precisely because Navidrome is *not*
    /// public, so its own URLs are never used as a fallback.
    Server,
}

impl CoverMode {
    /// Parses `COVER_MODE`. Anything unrecognised is an error rather than a
    /// silent fall-back to `off` — a typo that disables a feature you asked
    /// for is exactly the failure this variable is here to prevent.
    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "off" => Ok(Self::Off),
            "navidrome" => Ok(Self::Navidrome),
            "server" => Ok(Self::Server),
            other => Err(format!(
                "COVER_MODE must be one of: off, navidrome, server — got: {}",
                other
            )),
        }
    }
}

/// Whether a startup line is passing remark or something the operator must act
/// on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverStatus {
    /// Nothing is wrong; this is just where things stand.
    Info,
    /// Covers were asked for in a way that will not produce any.
    Warning,
}

/// The one line printed at startup about cover art.
///
/// It deliberately separates two situations that look identical to a newcomer
/// if phrased carelessly: "you have not set covers up, and here is how" must
/// not read as a failure on first run, while "you did set covers up, and they
/// will not appear" must. Conflating them is how an optional feature ends up
/// silently broken for months.
fn cover_status(mode: CoverMode, public_url: Option<&str>) -> (CoverStatus, String) {
    match (mode, public_url) {
        (CoverMode::Off, None) => (
            CoverStatus::Info,
            "Cover art: off — set COVER_MODE=navidrome (or server) and a public \
             NAVIDROME_PUBLIC_URL to enable it"
                .to_string(),
        ),
        // The operator reached for the origin without turning the feature on:
        // they clearly meant to have covers, so say plainly why there are none.
        (CoverMode::Off, Some(_)) => (
            CoverStatus::Warning,
            "Warning: NAVIDROME_PUBLIC_URL is set but COVER_MODE is off, so no \
             covers will be shown — add COVER_MODE=navidrome or COVER_MODE=server"
                .to_string(),
        ),
        (CoverMode::Navidrome, Some(public)) => (
            CoverStatus::Info,
            format!("Cover art: from Navidrome at {}", public),
        ),
        // Not a warning: a Navidrome with a `BaseUrl` configured already hands
        // back reachable URLs, so covers genuinely may work here.
        (CoverMode::Navidrome, None) => (
            CoverStatus::Info,
            "Cover art: from Navidrome itself — shown only if Navidrome is \
             publicly reachable (set NAVIDROME_PUBLIC_URL to pin the origin)"
                .to_string(),
        ),
        (CoverMode::Server, Some(public)) => (
            CoverStatus::Info,
            format!("Cover art: from the cover server at {}", public),
        ),
        // Unambiguously broken: `server` mode exists because Navidrome is not
        // exposed, so there is no fall-back URL that could possibly work.
        (CoverMode::Server, None) => (
            CoverStatus::Warning,
            "Warning: COVER_MODE=server but NAVIDROME_PUBLIC_URL is not set — \
             covers will not appear. Point it at the origin your tunnel or \
             reverse proxy publishes the cover server on"
                .to_string(),
        ),
    }
}

/// Everything the bridge needs to run, read once at startup.
struct Config {
    base_url: String,
    username: String,
    password: String,
    /// Where cover art comes from; `Off` unless the operator says otherwise.
    cover_mode: CoverMode,
    /// Public origin Navidrome is reachable at, if the operator set one.
    ///
    /// Discord downloads presence images from its own servers, so a URL on
    /// `localhost` (or any private address) is useless to it. When Navidrome
    /// hands back such a URL, we rewrite its origin to this one; without it
    /// cover art is skipped rather than sent broken.
    public_url: Option<String>,
    client_id: String,
    poll_interval: u64,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        fn required(key: &str) -> Result<String, String> {
            match std::env::var(key) {
                Ok(v) if !v.trim().is_empty() => Ok(v),
                _ => Err(format!("{} is not set", key)),
            }
        }

        // Normalised once at startup: no trailing slash, and no accidental
        // `base//rest/...` later.
        let base_url = std::env::var("NAVIDROME_URL")
            .unwrap_or_else(|_| "http://localhost:4533".to_string())
            .trim_end_matches('/')
            .to_string();

        let public_url = std::env::var("NAVIDROME_PUBLIC_URL")
            .ok()
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty())
            .map(|v| {
                if !(v.starts_with("http://") || v.starts_with("https://")) {
                    return Err(format!(
                        "NAVIDROME_PUBLIC_URL must be an absolute http(s) URL, got: {}",
                        v
                    ));
                }
                if v.starts_with("http://") {
                    // Not fatal — some setups do serve plain http — but it is
                    // worth saying out loud, because Discord may refuse to load
                    // a mixed-content image and the symptom would just be a
                    // missing cover with no explanation.
                    eprintln!(
                        "Warning: NAVIDROME_PUBLIC_URL is http:// — Discord may refuse \
                         to load it. Prefer https://"
                    );
                }
                Ok(v)
            })
            .transpose()?;

        Ok(Self {
            base_url,
            username: required("NAVIDROME_USER")?,
            password: required("NAVIDROME_PASSWORD")?,
            // A bad value is a hard error (see `CoverMode::parse`).
            cover_mode: CoverMode::parse(&std::env::var("COVER_MODE").unwrap_or_default())?,
            public_url,
            client_id: std::env::var("DISCORD_CLIENT_ID")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_DISCORD_CLIENT_ID.to_string()),
            poll_interval: std::env::var("POLL_INTERVAL")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&v| v > 0)
                .unwrap_or(1),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
struct NowPlaying {
    track_id: String,
    title: String,
    artist: String,
    album: String,
    /// Seconds already played.
    elapsed: i64,
    /// Total track length in seconds.
    duration: i64,
    playing: bool,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn track_from(entry: &Value, playing: bool) -> NowPlaying {
    NowPlaying {
        track_id: entry["id"].as_str().unwrap_or("").to_string(),
        title: entry["title"].as_str().unwrap_or("").to_string(),
        artist: entry["artist"]
            .as_str()
            .or_else(|| entry["displayArtist"].as_str())
            .unwrap_or("")
            .to_string(),
        album: entry["album"].as_str().unwrap_or("").to_string(),
        elapsed: entry["positionMs"].as_i64().unwrap_or(0) / 1000,
        duration: entry["duration"].as_i64().unwrap_or(0),
        playing,
    }
}

/// Timeout for a single Navidrome request. Without it a hung server would
/// block the polling loop forever and the status would silently freeze.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Percent-encodes a value for use inside a query string.
///
/// Everything we interpolate is user- or server-supplied: a username, a track
/// id, a salt. Characters like `&`, `#`, `%`, a space or non-ASCII would
/// silently corrupt the request (or bleed into other parameters).
fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

/// A fresh nonce for a single request.
///
/// The Subsonic spec asks for a random alphanumeric string of at least 12
/// characters; 16 hex chars from the kernel CSPRNG is comfortably inside that.
/// The salt is not a secret — its only job is to make every `t` unique — so a
/// failed `/dev/urandom` read degrades to a time-derived value rather than
/// taking the bridge down.
fn random_salt() -> String {
    let mut buf = [0u8; 8];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();

    if !ok {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        buf = (nanos as u64).to_le_bytes();
    }

    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Credentials for one Subsonic request.
///
/// Navidrome accepts either a plaintext password (`p=`) or a token pair
/// (`t=`/`s=`). We only ever build the token: `t` is
/// `md5(password + salt)` with a fresh salt each request, so **the password
/// itself never leaves this process** — not into a URL, not into the journal,
/// not into anything between us and Navidrome.
///
/// md5 is not a choice we made: the Subsonic spec mandates it for this
/// challenge-response, and here it authenticates without disclosing the
/// secret rather than protecting one at rest.
fn auth_params(config: &Config) -> String {
    auth_params_with_salt(config, &random_salt())
}

/// Same, with the salt supplied — so the digest can be checked against a
/// published md5 vector instead of only against md5 itself.
fn auth_params_with_salt(config: &Config, salt: &str) -> String {
    let digest = format!("{:x}", md5::compute(format!("{}{}", config.password, salt)));
    format!("u={}&t={}&s={}", encode_query(&config.username), digest, salt)
}

/// Builds a URL for one Subsonic endpoint. `extra` is query content we add
/// ourselves, such as `id=<track>`.
fn subsonic_url(config: &Config, endpoint: &str, extra: &str) -> String {
    let sep = if extra.is_empty() { "" } else { "&" };
    format!(
        "{}/rest/{}?{}{}{}&v=1.16.1&c=navidrome-discord-rpc&f=json",
        config.base_url,
        endpoint,
        auth_params(config),
        sep,
        extra,
    )
}

/// Performs a Subsonic GET and returns the parsed `subsonic-response` object.
///
/// Reporting the HTTP status first matters: a 401/500 body is HTML, not JSON,
/// and "Navidrome returned HTTP 401" is far more useful than "JSON parse
/// error" when the credentials are wrong.
fn subsonic_get(client: &Client, config: &Config, endpoint: &str, extra: &str) -> Result<Value, String> {
    let url = subsonic_url(config, endpoint, extra);

    let resp = client.get(&url).send().map_err(|e| {
        // `without_url` strips the request URL — query string included — out of
        // reqwest's message. The query carries the Subsonic token pair, which
        // is not the password and cannot be reversed into one, but an
        // access-log habit still has no place in a journal this process cannot
        // redact from later.
        format!("HTTP error talking to {}: {}", endpoint, e.without_url())
    })?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("Navidrome returned HTTP {}", status));
    }

    let text = resp
        .text()
        .map_err(|e| format!("HTTP body error: {}", e.without_url()))?;
    let body: Value =
        serde_json::from_str(&text).map_err(|e| format!("JSON parse error: {}", e))?;

    let response = &body["subsonic-response"];
    let status = response["status"].as_str().unwrap_or("");
    if status != "ok" {
        // Surface Navidrome's own message — "Wrong username or password" beats
        // "status not ok" by a mile when debugging.
        let code = response["error"]["code"].as_i64().unwrap_or(0);
        let message = response["error"]["message"].as_str().unwrap_or("no message given");
        return Err(format!("Navidrome API error {}: {}", code, message));
    }

    Ok(body)
}

/// Returns the currently playing track, `None` when nothing is playing,
/// or an error string when the request itself failed.
fn fetch_now_playing(client: &Client, config: &Config) -> Result<Option<NowPlaying>, String> {
    let body = subsonic_get(client, config, "getNowPlaying", "")?;
    parse_now_playing(&body)
}

/// Parses a Subsonic `getNowPlaying` response into the current track.
///
/// Returns `Ok(None)` when nothing is playing.
fn parse_now_playing(body: &Value) -> Result<Option<NowPlaying>, String> {
    let status = body["subsonic-response"]["status"].as_str().unwrap_or("");
    if status != "ok" {
        return Err(format!("Navidrome API status not ok: {}", status));
    }

    let np = &body["subsonic-response"]["nowPlaying"];
    if np.is_null() {
        return Ok(None);
    }

    // `nowPlaying.entry` is an array of tracks on Navidrome; some servers
    // return a single object instead. Resolve the track first, then read the
    // play state *from the track* — reading it from the array container always
    // yields null, which made a paused track look like it was still playing.
    let entry = match &np["entry"] {
        Value::Array(tracks) => match tracks.first() {
            Some(first) => first,
            None => return Ok(None),
        },
        Value::Object(_) => &np["entry"],
        _ => return Ok(None),
    };

    if entry["title"].is_null() {
        return Ok(None);
    }

    // Older servers may omit `state`; assume playing in that case.
    let playing = entry["state"]
        .as_str()
        .map(|s| s.eq_ignore_ascii_case("playing"))
        .unwrap_or(true);

    Ok(Some(track_from(entry, playing)))
}

/// Splits `http://host:port/path?query` into its origin and the rest.
fn split_origin(url: &str) -> Option<(&str, &str)> {
    let scheme_end = url.find("://")?;
    let host_start = scheme_end + 3;
    let host_end = url[host_start..]
        .find('/')
        .map(|i| host_start + i)
        .unwrap_or(url.len());
    Some((&url[..host_end], &url[host_end..]))
}

/// True when `origin` names a host Discord's servers could not reach — a
/// loopback address, a private range, or an mDNS name. Discord downloads
/// presence images itself, so `localhost` or `192.168.1.42` would simply
/// never resolve for it.
fn is_locally_only(origin: &str) -> bool {
    let after_scheme = origin.split_once("://").map(|(_, h)| h).unwrap_or(origin);
    // Drop userinfo, then port — and mind IPv6 brackets (`[::1]:4533`).
    let host = after_scheme.rsplit('@').next().unwrap_or(after_scheme);
    // Decided before the split below, which eats the very colons that tell an
    // IPv6 literal (`fd00::1`) from a hostname with a port (`host:4533`).
    let is_ipv6_literal = host.contains('[') || host.matches(':').count() > 1;
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        host.split(':').next().unwrap_or(host)
    };
    let host = host.to_ascii_lowercase();
    // A fully-qualified `localhost.` names the same host; leaving the dot in
    // would let it past every check below.
    let host = host.strip_suffix('.').unwrap_or(&host);

    let in_172_private = host
        .strip_prefix("172.")
        .and_then(|rest| rest.split('.').next())
        .and_then(|octet| octet.parse::<u8>().ok())
        .is_some_and(|octet| (16..=31).contains(&octet));

    // `fc`/`fd` — the fc00::/7 unique-local range — is only ever applied to
    // IPv6 literals (see above); as a plain hostname prefix it also swallows
    // public names like `fcmusic.example.com`, throwing away a cover that
    // would have worked.
    host.is_empty()
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host == "0.0.0.0"
        || host == "::1"
        || host == "ip6-localhost"
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.starts_with("169.254.")
        || in_172_private
        || (is_ipv6_literal && (host.starts_with("fc") || host.starts_with("fd")))
}

/// Turns a cover URL Navidrome produced into one Discord can actually fetch.
///
/// Navidrome builds these URLs from the Host header of the request that asked
/// for them — and we ask it on `localhost` — so what comes back is unreachable
/// for Discord, which fetches presence images from its own servers. If the
/// operator declared `NAVIDROME_PUBLIC_URL` we swap that origin in; otherwise
/// we keep the URL only when Navidrome already returned a publicly reachable
/// one (it will have a `BaseUrl` configured), and drop it silently when not.
///
/// `CoverMode::Server` is the exception: in that mode Navidrome is private by
/// construction — publishing it would defeat the point of running a separate
/// cover server — so its own URL is never offered as a fallback. Handing
/// Discord an address we already know is unreachable would just produce a
/// missing image with no explanation, which is worse than no image at all.
fn public_image_url(raw: &str, public_url: Option<&str>, mode: CoverMode) -> Option<String> {
    let (origin, rest) = split_origin(raw)?;

    if let Some(public) = public_url {
        return Some(format!("{}{}", public, rest));
    }

    if mode == CoverMode::Server {
        return None;
    }

    if is_locally_only(origin) {
        None
    } else {
        Some(raw.to_string())
    }
}

/// Resolves a cover image for a track.
///
/// Returns `Ok(None)` when there is genuinely no artwork to show, and `Err`
/// when the request failed — the caller logs the latter and stays quiet about
/// the former.
///
/// `getAlbumInfo` takes an album **or song** id, so this keys off the track id
/// we already hold rather than `albumId`, which the `nowPlaying` payload does
/// not promise to carry. The URLs it returns (`/share/img/<JWT>`) are signed
/// but **credential-free** — no username, no password, no token — which is
/// exactly what may be handed to a third party like Discord.
fn fetch_cover(client: &Client, config: &Config, track_id: &str) -> Result<Option<String>, String> {
    // Covers off means the request is never made: the mode is the switch, so
    // an operator who configured no public endpoint pays nothing per track
    // instead of a wasted `getAlbumInfo` round-trip every time the song
    // changes.
    if config.cover_mode == CoverMode::Off || track_id.is_empty() {
        return Ok(None);
    }

    let body = subsonic_get(
        client,
        config,
        "getAlbumInfo",
        &format!("id={}", encode_query(track_id)),
    )?;

    // Prefer the largest variant: it is a few hundred KB, and Discord
    // downscales it anyway. Fall back to medium/small so an album with only a
    // thumbnail still shows something rather than nothing.
    let raw = body["subsonic-response"]["albumInfo"]
        .get("largeImageUrl")
        .or_else(|| body["subsonic-response"]["albumInfo"].get("mediumImageUrl"))
        .or_else(|| body["subsonic-response"]["albumInfo"].get("smallImageUrl"))
        .and_then(Value::as_str);

    let Some(raw) = raw else {
        return Ok(None);
    };

    Ok(public_image_url(raw, config.public_url.as_deref(), config.cover_mode))
}

/// The presence fields we compute for a track, kept separate from the Discord
/// `Activity` type so the logic is unit-testable.
#[derive(Debug, Clone, PartialEq)]
struct Presence {
    details: String,
    state: String,
    start: Option<i64>,
    end: Option<i64>,
    /// Album name, used as the tooltip on the cover.
    album: String,
    /// Cover image Discord is able to fetch, when one was resolved.
    ///
    /// Kept out of `for_track` because resolving it costs a request of its
    /// own, which the polling loop does once per track rather than once a
    /// second — see `fetch_cover`.
    cover: Option<String>,
}

impl Presence {
    /// Builds the presence for a track.
    ///
    /// Discord draws the progress bar from `now - start` and has no "paused"
    /// state, so a timestamp can never be made to *stand still*: sending one
    /// keeps the bar moving. Therefore:
    ///
    /// * **playing** — send a `start`/`end` window anchored to Navidrome's live
    ///   `positionMs`; Discord advances the bar itself.
    /// * **paused** — send **no timestamps at all**, so nothing moves; the state
    ///   text carries the `⏸ Paused` marker instead.
    fn for_track(track: &NowPlaying, now: i64) -> Self {
        let credits = format!("{} — {}", track.artist, track.album)
            .trim_end_matches(" — ")
            .to_string();
        let credits = if credits.is_empty() {
            "Unknown artist".to_string()
        } else {
            credits
        };

        let (start, end, state) = if track.playing {
            let window = if track.duration > 0 {
                // Navidrome can briefly report a position past the end of the
                // track while it is still transitioning. Anchoring to that would
                // produce an inverted window (end before start), which Discord
                // renders as an empty or nonsense bar.
                let elapsed = track.elapsed.clamp(0, track.duration);
                let start = now - elapsed;
                (Some(start), Some(start + track.duration))
            } else {
                // Length unknown: no progress bar possible.
                (None, None)
            };
            (window.0, window.1, credits)
        } else {
            (None, None, format!("⏸ Paused · {}", credits))
        };

        Self {
            details: track.title.clone(),
            state,
            start,
            end,
            album: track.album.clone(),
            cover: None,
        }
    }

    /// Builds the Discord activity that is sent over IPC.
    ///
    /// `SET_ACTIVITY` *assigns* the whole activity (it replaces the previous
    /// one, which is why clearing works by sending `activity: null`), so a
    /// paused presence omits `timestamps` entirely — the stale bar is dropped
    /// rather than silently kept, because the field is simply absent from the
    /// payload (the crate skips `None`).
    fn to_activity(&self) -> Activity<'_> {
        let mut activity = Activity::new()
            .activity_type(ActivityType::Listening)
            .status_display_type(StatusDisplayType::Details)
            .details(self.details.clone())
            .state(self.state.clone());

        if let (Some(start), Some(end)) = (self.start, self.end) {
            activity = activity.timestamps(Timestamps::new().start(start).end(end));
        }

        // The album cover, with the album name as its hover text. Discord
        // fetches `large_image` from its own servers, so this URL only ever
        // holds something reachable from outside this machine — `fetch_cover`
        // is what guarantees that, and simply finds nothing when it cannot.
        if let Some(cover) = &self.cover {
            let mut assets = Assets::new().large_image(cover.clone());
            if !self.album.is_empty() {
                assets = assets.large_text(self.album.clone());
            }
            activity = activity.assets(assets);
        }

        // Two links under the status: Navidrome itself, and this bridge.
        // They are static, so they belong on the paused presence too.
        //
        // Discord shows buttons to *other* users only — the account that set
        // the activity never sees them, which is documented behaviour, not a
        // bug in this bridge.
        activity = activity.buttons(vec![
            Button::new("Navidrome", NAVIDROME_LINK),
            Button::new("Source", REPOSITORY_LINK),
        ]);

        activity
    }
}

/// Hand-rolled Discord IPC transport.
///
/// The `discord-rich-presence` client only ever *writes* to the socket: its
/// `set_activity` returns `Ok` the moment the bytes reach the kernel, and it
/// never reads Discord's replies — not even the `READY` that follows the
/// handshake. Left unread, those replies pile up in the socket's receive
/// buffer until it fills, at which point Discord stops delivering updates
/// while every write still reports success. That is exactly the "the log
/// says `[paused]` but the status never changes" failure.
///
/// So this transport reads every frame Discord sends back, matches replies by
/// `nonce`, and uses a read timeout so a silent client cannot block the loop.
mod ipc {
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::time::Duration;

    const READ_TIMEOUT: Duration = Duration::from_secs(2);

    /// Ceiling on a single reply.
    ///
    /// The length prefix is whatever the peer claims, and the body is allocated
    /// to match. Discord's replies are a few kilobytes at most, so an
    /// unchecked `u32` would let a hostile or merely confused peer on the other
    /// end ask this process to allocate four gigabytes.
    const MAX_FRAME_BYTES: usize = 1024 * 1024;

    pub struct Ipc {
        sock: UnixStream,
        nonce: u64,
    }

    impl Ipc {
        pub fn connect(client_id: &str) -> Result<Self, String> {
            let path = find_pipe().ok_or_else(|| {
                "Discord IPC socket not found (is the Discord client running?)".to_string()
            })?;
            let sock = UnixStream::connect(&path)
                .map_err(|e| format!("connect {}: {}", path.display(), e))?;

            let mut ipc = Self::from_stream(sock)?;

            let hello = json!({ "v": 1, "client_id": client_id });
            ipc.write_frame(0, &hello.to_string())?;

            // Read the READY dispatch. Skipping it is part of what used to
            // leave the receive buffer growing forever.
            let reply = ipc.read_frame()?;
            match reply.get("evt").and_then(|v| v.as_str()) {
                Some("READY") => Ok(ipc),
                other => Err(format!(
                    "unexpected handshake reply (evt={:?}): {}",
                    other, reply
                )),
            }
        }

        fn from_stream(sock: UnixStream) -> Result<Self, String> {
            sock.set_read_timeout(Some(READ_TIMEOUT))
                .map_err(|e| format!("set_read_timeout: {}", e))?;
            Ok(Self { sock, nonce: 0 })
        }

        /// Sends an activity (`Value::Null` clears it) and waits for Discord's
        /// answer, returning the `data` Discord echoed back.
        pub fn set_activity(&mut self, activity: Value) -> Result<Value, String> {
            let nonce = format!("{}-{}", std::process::id(), self.next_nonce());
            let frame = json!({
                "cmd": "SET_ACTIVITY",
                "args": { "pid": std::process::id(), "activity": activity },
                "nonce": nonce,
            });
            self.write_frame(1, &frame.to_string())?;

            // Discord pushes unsolicited DISPATCH events too, so keep reading
            // until the frame carrying our nonce shows up.
            loop {
                let reply = self.read_frame()?;
                if reply.get("nonce").and_then(|v| v.as_str()) != Some(nonce.as_str()) {
                    continue;
                }
                if reply.get("evt").and_then(|v| v.as_str()) == Some("ERROR") {
                    return Err(format!(
                        "Discord rejected the activity: {}",
                        reply.get("data").cloned().unwrap_or(Value::Null)
                    ));
                }
                return Ok(reply.get("data").cloned().unwrap_or(Value::Null));
            }
        }

        fn next_nonce(&mut self) -> u64 {
            self.nonce += 1;
            self.nonce
        }

        fn write_frame(&mut self, opcode: u32, payload: &str) -> Result<(), String> {
            let data = payload.as_bytes();
            let mut header = Vec::with_capacity(8);
            header.extend_from_slice(&opcode.to_le_bytes());
            header.extend_from_slice(&(data.len() as u32).to_le_bytes());
            self.sock.write_all(&header).map_err(|e| e.to_string())?;
            self.sock.write_all(data).map_err(|e| e.to_string())
        }

        fn read_frame(&mut self) -> Result<Value, String> {
            let mut header = [0u8; 8];
            self.sock.read_exact(&mut header).map_err(|e| map_read_error(e, "header"))?;
            let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
            if len > MAX_FRAME_BYTES {
                return Err(format!(
                    "Discord sent an implausibly large frame ({} bytes)",
                    len
                ));
            }
            let mut body = vec![0u8; len];
            self.sock.read_exact(&mut body).map_err(|e| map_read_error(e, "body"))?;
            let text =
                String::from_utf8(body).map_err(|_| "non-UTF8 reply from Discord".to_string())?;
            serde_json::from_str(&text).map_err(|e| format!("bad JSON reply: {}", e))
        }

        #[cfg(test)]
        fn pair() -> (Self, UnixStream) {
            let (a, b) = UnixStream::pair().expect("unix socket pair");
            (Self::from_stream(a).expect("timeout set"), b)
        }
    }

    fn map_read_error(e: std::io::Error, what: &str) -> String {
        if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut {
            "timed out waiting for a reply from Discord".to_string()
        } else {
            format!("read {}: {}", what, e)
        }
    }

    fn find_pipe() -> Option<PathBuf> {
        // Mirrors Discord's own lookup order: XDG first, then the usual temp
        // dirs, and the Flatpak/Snap sub-paths inside each.
        const ENV_KEYS: [&str; 4] = ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"];
        const SUBPATHS: [&str; 5] = [
            "",
            "app/com.discordapp.Discord/",
            "app/dev.vencord.Vesktop/",
            ".flatpak/com.discordapp.Discord/xdg-run/",
            ".flatpak/dev.vencord.Vesktop/xdg-run/",
        ];

        for key in ENV_KEYS {
            let Ok(base) = std::env::var(key) else { continue };
            let base = PathBuf::from(base);
            if !base.is_dir() {
                continue;
            }
            for i in 0..10 {
                for sub in SUBPATHS {
                    let candidate = base.join(sub).join(format!("discord-ipc-{}", i));
                    if candidate.exists() {
                        return Some(candidate);
                    }
                }
            }
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        use std::thread;

        /// Reads one frame from the "Discord" side of the pair.
        fn read_frame(sock: &mut UnixStream) -> Value {
            let mut header = [0u8; 8];
            sock.read_exact(&mut header).expect("read header");
            let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
            let mut body = vec![0u8; len];
            sock.read_exact(&mut body).expect("read body");
            serde_json::from_slice(&body).expect("parse frame")
        }

        fn write_frame(sock: &mut UnixStream, opcode: u32, value: &Value) {
            let payload = value.to_string();
            let data = payload.as_bytes();
            sock.write_all(&opcode.to_le_bytes()).expect("write opcode");
            sock.write_all(&(data.len() as u32).to_le_bytes()).expect("write len");
            sock.write_all(data).expect("write body");
        }

        #[test]
        fn set_activity_reads_the_matching_reply() {
            let (mut ipc, mut peer) = Ipc::pair();

            // Pretend to be Discord: an unrelated DISPATCH first, then the real
            // reply carrying our nonce.
            let responder = thread::spawn(move || {
                let request = read_frame(&mut peer);
                assert_eq!(request["cmd"], "SET_ACTIVITY");
                let nonce = request["nonce"].clone();

                write_frame(
                    &mut peer,
                    1,
                    &json!({"cmd": "DISPATCH", "evt": "SPECTATE_GAME", "nonce": null,
                            "data": {"id": "noise"}}),
                );
                write_frame(
                    &mut peer,
                    1,
                    &json!({"cmd": "SET_ACTIVITY", "evt": null, "nonce": nonce,
                            "data": {"details": "accepted"}}),
                );
            });

            let echoed = ipc.set_activity(json!({"details": "Black Knife"})).unwrap();
            assert_eq!(echoed["details"], "accepted");
            responder.join().expect("responder thread");
        }

        #[test]
        fn set_activity_surfaces_a_discord_error() {
            let (mut ipc, mut peer) = Ipc::pair();

            let responder = thread::spawn(move || {
                let request = read_frame(&mut peer);
                let nonce = request["nonce"].clone();
                write_frame(
                    &mut peer,
                    1,
                    &json!({"cmd": "SET_ACTIVITY", "evt": "ERROR", "nonce": nonce,
                            "data": {"code": 4000, "message": "Invalid activity"}}),
                );
            });

            let err = ipc.set_activity(json!({"details": "x"})).unwrap_err();
            assert!(err.contains("Discord rejected the activity"), "err was: {}", err);
            assert!(err.contains("Invalid activity"), "err was: {}", err);
            responder.join().expect("responder thread");
        }

        #[test]
        fn nonces_are_distinct_per_request() {
            let (mut ipc, _peer) = Ipc::pair();
            assert_eq!(ipc.next_nonce(), 1);
            assert_eq!(ipc.next_nonce(), 2);
            assert_eq!(ipc.next_nonce(), 3);
        }
    }
}

/// Thin wrapper around the Discord IPC connection that reconnects on demand.
struct Rpc {
    client_id: String,
    client: Option<ipc::Ipc>,
}

impl Rpc {
    fn new(client_id: String) -> Self {
        Self {
            client_id,
            client: None,
        }
    }

    fn ensure_connected(&mut self) -> Result<(), String> {
        if self.client.is_some() {
            return Ok(());
        }
        let client = ipc::Ipc::connect(&self.client_id)?;
        self.client = Some(client);
        Ok(())
    }

    /// Pushes an activity and returns what Discord echoed back, so callers can
    /// tell an accepted presence from one that only reached the socket.
    fn set_activity(&mut self, activity: Activity) -> Result<Value, String> {
        self.ensure_connected()?;
        let value = serde_json::to_value(&activity).map_err(|e| e.to_string())?;
        match self.client.as_mut().expect("client is connected").set_activity(value) {
            Ok(data) => Ok(data),
            Err(e) => {
                // Drop the broken client so the next call reconnects.
                self.client = None;
                Err(e)
            }
        }
    }

    fn clear_activity(&mut self) -> Result<(), String> {
        if self.client.is_none() {
            return Ok(());
        }
        match self.client.as_mut().expect("client is connected").set_activity(Value::Null) {
            Ok(_) => Ok(()),
            Err(e) => {
                self.client = None;
                Err(e)
            }
        }
    }
}

/// The last presence we pushed, so we only log real changes.
struct Current {
    /// Everything that requires a resend when it changes — `playing` included,
    /// so pausing re-sends the presence.
    key: String,
    /// Identity of the track the stored `cover` belongs to. Kept separate from
    /// `key` on purpose: pausing must not spend another request re-resolving
    /// art that has not changed — only switching tracks should.
    track_key: String,
    cover: Option<String>,
}

/// How long to keep showing the last track after Navidrome reports nothing.
/// Navidrome briefly returns an empty `nowPlaying` between tracks; clearing the
/// status immediately made the presence flicker, so we wait before clearing.
const NOTHING_PLAYING_GRACE: i64 = 12;

fn print_usage() {
    println!(
        "navidrome-discord-rpc {}\n\
         \n\
         Shows the track playing on Navidrome as Discord Rich Presence.\n\
         \n\
         Usage:\n\
         \x20 navidrome-discord-rpc            Start the bridge (needs the variables below)\n\
         \x20 navidrome-discord-rpc serve-covers  Run the album-art server instead\n\
         \n\
         {}\n\
         \n\
         Flags:\n\
         \x20 -h, --help     Show this help\n\
         \x20 -V, --version  Show the version",
        env!("CARGO_PKG_VERSION"),
        USAGE
    );
}

// ---- Cover server -------------------------------------------------------
//
// A deliberately narrow HTTP server: it hands out album artwork and nothing
// else. It exists because Discord fetches presence images from its own
// servers, and opening Navidrome to the internet just to serve a JPEG would
// publish a login form and an API alongside it. So this listens on loopback
// only and is published through Tailscale Funnel, which terminates TLS in
// front of it and is the sole thing that ever talks to it.
//
// It holds no credentials on purpose. The `/share/img/<JWT>` URLs it proxies
// are signed but carry no username or password — Navidrome serves them without
// authentication — so a compromise of this process cannot reach the account,
// cannot list the library, and cannot reach any Navidrome endpoint other than
// the single image path it is willing to construct.

/// Loopback by default. The only intended client is the Funnel relay running
/// on this machine; binding wider would publish a server nobody asked for to
/// the local network.
const DEFAULT_COVERS_LISTEN: &str = "127.0.0.1:4534";

/// Upper bound on cached artwork. Covers are tens of KB each, so this tops out
/// at a few megabytes — the cap exists to bound memory against a client that
/// walks every token it can guess, not because the images are large.
const COVER_CACHE_MAX: usize = 256;

/// How long a failed lookup is remembered. Without this, a flood of invented
/// paths would turn the server into a load generator pointed at Navidrome.
const COVER_MISS_TTL: Duration = Duration::from_secs(60);

/// Cap on how much of a request we are willing to read. A browser or Discord
/// fetching a picture sends a few hundred bytes; anything approaching this is
/// not that.
const COVER_HEAD_MAX: usize = 8 * 1024;

/// Hard ceiling on a single upstream image, so a compromised or misbehaving
/// Navidrome cannot exhaust memory through us. 8 MB is far beyond any real
/// album cover — the ones here are ~20 KB.
const COVER_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Ceiling on the artwork cache as a whole.
///
/// The entry count alone is not a memory bound: `COVER_CACHE_MAX` entries at
/// `COVER_MAX_BYTES` each would be two gigabytes. Evicting on total size makes
/// the worst case a fixed number rather than a multiplication.
const COVER_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Ceiling on remembered failures.
///
/// Without it this map grows without limit: any well-formed token that does not
/// exist adds an entry, and this server is reachable from the internet.
const COVER_MISS_MAX: usize = 1024;

/// How many connections may be in flight at once.
///
/// The server is publicly reachable, spawns a thread per connection, and each
/// thread carries a 2 MB stack by default — so an unbounded count is a
/// memory-exhaustion lever for anyone who finds the port, sharpened by the read
/// timeout letting each held connection occupy a thread for ten seconds.
/// Discord fetches a handful of images at a time; this is far above real load
/// and exists only to bound the worst case.
const COVER_MAX_CONNECTIONS: usize = 32;

/// Content types we are willing to republish.
///
/// `image/*` is broader than it reads: `image/svg+xml` carries scripts, and
/// serving one from a domain other people trust is a favour nobody asked for.
/// These are the raster formats Navidrome actually produces for artwork.
const ALLOWED_COVER_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

struct CoverCacheEntry {
    bytes: Arc<Vec<u8>>,
    content_type: String,
}

struct CoverServer {
    /// Navidrome origin, without a trailing slash.
    upstream: String,
    client: Client,
    hits: Mutex<HashMap<String, CoverCacheEntry>>,
    /// Path -> when it last failed.
    misses: Mutex<HashMap<String, Instant>>,
}

/// Validates a request target down to exactly the upstream path we are willing
/// to fetch.
///
/// Returns `None` for anything that is not a well-formed artwork request. This
/// is the whole security boundary of the server: it is what keeps a request
/// from steering the upstream fetch onto another Navidrome endpoint, out of
/// the share directory, or into a header through an injected newline.
fn cover_upstream_path(target: &str) -> Option<String> {
    // Only origin-form targets. An absolute-form target ("GET http://host/…")
    // would name an origin we did not choose, so refuse it outright.
    if target.contains("://") || target.contains('#') || target.contains(' ') {
        return None;
    }

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };

    // Two accepted prefixes, because it is not obvious whether the Funnel
    // handler forwards the mounted path or strips it — accepting both costs
    // nothing and removes a whole class of "works locally, 404s in prod".
    let token = path
        .strip_prefix("/share/img/")
        .or_else(|| path.strip_prefix("/img/"))?;

    // Navidrome's token is base64url segments joined by dots. Anything outside
    // that alphabet — a slash, a dot-dot, a percent escape, a newline — is not
    // a token we could have handed out.
    let token_ok = !token.is_empty()
        && !token.contains("..")
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !token_ok {
        return None;
    }

    let query = match query {
        None | Some("") => String::new(),
        Some(q) => {
            // Bounded in both length and charset: enough for the `size=600`
            // Navidrome actually emits, while excluding anything that could
            // inject a header, escape into another path, or name another
            // origin. Extra well-formed parameters are forwarded and ignored
            // upstream — the path is already pinned to the share endpoint.
            let query_ok = q.len() <= 64
                && q
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '=' | '&' | '.' | '-'));
            if !query_ok {
                return None;
            }
            format!("?{}", q)
        }
    };

    Some(format!("/share/img/{}{}", token, query))
}

impl CoverServer {
    fn new(upstream: String) -> Self {
        Self {
            upstream,
            client: Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .unwrap_or_else(|_| Client::new()),
            hits: Mutex::new(HashMap::new()),
            misses: Mutex::new(HashMap::new()),
        }
    }

    /// Fetches from Navidrome and keeps only something that is actually an
    /// image. An error page served with a 200 would otherwise be published to
    /// the internet as if it were artwork.
    fn fetch(&self, path: &str) -> Option<CoverCacheEntry> {
        let resp = self.client.get(format!("{}{}", self.upstream, path)).send().ok()?;
        if !resp.status().is_success() {
            return None;
        }

        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();

        let media_type = allowed_cover_type(&content_type)?;

        let bytes = resp.bytes().ok()?;
        if bytes.is_empty() || bytes.len() > COVER_MAX_BYTES {
            return None;
        }

        // Crop to the square Discord's card expects, over bytes that are
        // already in hand. `None` means "leave it alone", not "drop it": a
        // cover that arrives uncropped is a lesser evil than a missing one.
        let (bytes, content_type) = match to_square(&bytes) {
            Some((cropped, encoded_as)) if cropped.len() <= COVER_MAX_BYTES => {
                (Arc::new(cropped), encoded_as.to_string())
            }
            _ => (Arc::new(bytes.to_vec()), media_type),
        };

        Some(CoverCacheEntry {
            bytes,
            content_type,
        })
    }

    fn respond(&self, method: &str, target: &str) -> Vec<u8> {
        if method != "GET" && method != "HEAD" {
            return error_response(
                405,
                "method not allowed",
                Some("Allow: GET, HEAD\r\n"),
                method == "HEAD",
            );
        }
        let wants_head = method == "HEAD";

        // Unparseable paths never reach Navidrome: answering here is both
        // faster and what stops the server being used to poke at upstream.
        let Some(path) = cover_upstream_path(target) else {
            return error_response(404, "not found", None, wants_head);
        };

        if let Some(entry) = self.hits.lock().expect("cache lock").get(&path) {
            return image_response(entry, wants_head);
        }

        // Recently failed: answer without asking Navidrome again.
        let recently_missed = self
            .misses
            .lock()
            .expect("miss lock")
            .get(&path)
            .is_some_and(|at| at.elapsed() < COVER_MISS_TTL);
        if recently_missed {
            return error_response(404, "not found", None, wants_head);
        }

        let Some(entry) = self.fetch(&path) else {
            let mut misses = self.misses.lock().expect("miss lock");
            // Capped for the same reason the artwork cache is: this map is
            // keyed by whatever a client chose to ask for, so without a bound
            // anyone probing the public endpoint grows it without limit.
            while misses.len() >= COVER_MISS_MAX {
                let Some(oldest) = misses.keys().next().cloned() else {
                    break;
                };
                misses.remove(&oldest);
            }
            misses.insert(path, Instant::now());
            return error_response(404, "not found", None, wants_head);
        };

        // Built before the entry is moved into the cache. Reading it back out
        // afterwards would mean picking an arbitrary value from a HashMap —
        // which is how one listener ends up serving another track's cover.
        let response = image_response(&entry, wants_head);

        {
            let mut hits = self.hits.lock().expect("cache lock");
            // Evict arbitrarily rather than implementing an LRU: refetching a
            // cover is a local request, and predictability beats cleverness in
            // a component that faces the internet. Both bounds are needed —
            // the entry count for bookkeeping, the total size for memory.
            while hits.len() >= COVER_CACHE_MAX {
                let Some(oldest) = hits.keys().next().cloned() else {
                    break;
                };
                hits.remove(&oldest);
            }
            hits.insert(path, entry);
            while !hits.is_empty() && total_cached_bytes(&hits) > COVER_CACHE_BYTES {
                let Some(oldest) = hits.keys().next().cloned() else {
                    break;
                };
                hits.remove(&oldest);
            }
        }

        response
    }
}

/// Total bytes held by the artwork cache — the bound that actually matters for
/// memory, since entry count on its own multiplies with whatever a single image
/// is allowed to weigh.
fn total_cached_bytes(hits: &HashMap<String, CoverCacheEntry>) -> usize {
    hits.values().map(|entry| entry.bytes.len()).sum()
}

/// Normalises a `Content-Type` header to a media type we are willing to
/// republish, or `None` when it is not one.
///
/// Parameters are not part of the media type — and only the media type is
/// allowlisted, because `image/*` is broader than it reads: `image/svg+xml`
/// carries scripts, and serving one from a domain other people trust is a
/// favour nobody asked for.
fn allowed_cover_type(header: &str) -> Option<String> {
    let media_type = header
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    ALLOWED_COVER_TYPES
        .contains(&media_type.as_str())
        .then_some(media_type)
}

/// Crops an image to a centred square, returning the new bytes and the media
/// type they were encoded as.
///
/// Discord renders `large_image` inside a square box and recommends square
/// assets of its own, while Navidrome returns covers at the album's native
/// aspect — 1200x675 for the album playing when this was written. Navidrome
/// cannot crop: its `square=true` parameter pads with black bars, which is the
/// original letterboxing with an extra round-trip added. So the crop happens
/// here, over bytes that are already in memory.
///
/// Returns `None` when there is nothing to do — the image is already square,
/// has no pixels, or is in a format this build cannot decode — and the caller
/// serves the original untouched. A cover that arrives uncropped is a lesser
/// evil than no cover at all.
fn to_square(bytes: &[u8]) -> Option<(Vec<u8>, &'static str)> {
    let source = image::load_from_memory(bytes).ok()?;

    let (width, height) = (source.width(), source.height());
    let side = width.min(height);
    if side == 0 || width == height {
        return None;
    }

    // Decided from the source, not from the crop: `crop_imm` hands back an
    // RGBA buffer whatever went in, so asking it about alpha would say yes
    // every time and force every cover through PNG.
    let had_alpha = source.color().has_alpha();

    let x = (width - side) / 2;
    let y = (height - side) / 2;
    let cropped: image::DynamicImage =
        image::imageops::crop_imm(&source, x, y, side, side).to_image().into();

    let mut encoded = Vec::new();
    let encoded_as = if had_alpha {
        // PNG, because JPEG has nowhere to put alpha and would flatten it to
        // black — which is the very failure this crop exists to avoid.
        cropped
            .write_to(&mut std::io::Cursor::new(&mut encoded), image::ImageFormat::Png)
            .ok()?;
        "image/png"
    } else {
        // JPEG for opaque art: what a photo compresses to, and what Navidrome
        // itself sends for resized covers. Converted explicitly rather than
        // handed through as a DynamicImage, whose color type is not ours to
        // guess at.
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 85)
            .encode_image(&cropped.to_rgb8())
            .ok()?;
        "image/jpeg"
    };

    Some((encoded, encoded_as))
}

fn image_response(entry: &CoverCacheEntry, head_only: bool) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         Cache-Control: public, max-age=86400\r\nX-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n",
        entry.content_type,
        entry.bytes.len()
    )
    .into_bytes();

    if !head_only {
        out.extend_from_slice(&entry.bytes);
    }
    out
}

fn error_response(status: u16, reason: &str, extra: Option<&str>, head_only: bool) -> Vec<u8> {
    let body = format!("{} {}\n", status, reason);
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n{}\r\n",
        status,
        reason,
        if head_only { 0 } else { body.len() },
        extra.unwrap_or("")
    )
    .into_bytes();

    if !head_only {
        out.extend_from_slice(body.as_bytes());
    }
    out
}

/// Reads up to the end of the request head. Bounded, and gives up rather than
/// accumulating whatever a client keeps sending.
fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];

    while buf.len() < COVER_HEAD_MAX {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() >= COVER_HEAD_MAX {
            break;
        }
    }

    if buf.is_empty() {
        return None;
    }
    String::from_utf8(buf).ok()
}

fn handle_connection(server: &CoverServer, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(HTTP_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HTTP_TIMEOUT));

    let Some(head) = read_head(&mut stream) else {
        return;
    };

    // Only the request line matters: everything else in the head is ignored,
    // so nothing a client sends can influence what is served.
    let Some(request_line) = head.lines().next() else {
        let _ = stream.write_all(&error_response(400, "bad request", None, false));
        return;
    };

    let mut parts = request_line.split_whitespace();
    let (method, target) = match (parts.next(), parts.next()) {
        (Some(m), Some(t)) => (m.to_string(), t.to_string()),
        _ => {
            let _ = stream.write_all(&error_response(400, "bad request", None, false));
            return;
        }
    };

    let response = server.respond(&method, &target);
    let _ = stream.write_all(&response);
    let _ = stream.flush();
}

/// Runs the cover server. Does not return.
fn serve_covers() -> ! {
    // Deliberately reads no credentials: see the note atop this section.
    let upstream = std::env::var("NAVIDROME_URL")
        .unwrap_or_else(|_| "http://localhost:4533".to_string())
        .trim_end_matches('/')
        .to_string();
    let listen = std::env::var("COVERS_LISTEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_COVERS_LISTEN.to_string());

    let listener = match TcpListener::bind(&listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Cover server failed to bind {}: {}", listen, e);
            std::process::exit(2);
        }
    };

    let server = Arc::new(CoverServer::new(upstream.clone()));
    let active = Arc::new(AtomicUsize::new(0));

    println!("Cover server starting (album art only, no credentials held)");
    println!("  Listening on: {}", listen);
    println!("  Upstream:     {}/share/img/<token>", upstream);

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let server = Arc::clone(&server);
        let active = Arc::clone(&active);

        if active.fetch_add(1, Ordering::Relaxed) >= COVER_MAX_CONNECTIONS {
            // At the cap: drop the connection rather than queue it. A queue
            // would hold the caller's connection open — which is the very
            // resource being spent — and just as effectively.
            active.fetch_sub(1, Ordering::Relaxed);
            continue;
        }

        let slot = ConnectionSlot(Arc::clone(&active));
        // A thread per connection: an upstream fetch can take as long as the
        // HTTP timeout, and one slow image must not stall the rest.
        thread::spawn(move || {
            handle_connection(&server, stream);
            drop(slot);
        });
    }

    // `incoming` only ends if the listener itself fails.
    eprintln!("Cover server listener stopped unexpectedly");
    std::process::exit(1);
}

/// Releases one of the bounded connection slots when dropped.
///
/// A `Drop` rather than a decrement at the end of the handler, so that a panic
/// in a handler thread cannot leak a slot and quietly close the server to
/// legitimate traffic over time.
struct ConnectionSlot(Arc<AtomicUsize>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("-h" | "--help") => {
            print_usage();
            return;
        }
        Some("-V" | "--version") => {
            println!("navidrome-discord-rpc {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("serve-covers") => {
            serve_covers();
        }
        Some(other) => {
            eprintln!("Unknown argument: {}\n", other);
            print_usage();
            std::process::exit(2);
        }
        None => {}
    }

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error: {}", e);
            eprintln!("\n{}", USAGE);
            std::process::exit(2);
        }
    };

    let client = match Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to build HTTP client: {}", e);
            std::process::exit(2);
        }
    };

    println!("Navidrome -> Discord Rich Presence starting");
    println!("  Navidrome: {}", config.base_url);
    println!("  User: {}", config.username);
    println!("  Poll interval: {}s", config.poll_interval);
    println!("  Discord client_id: {}", config.client_id);
    // Being honest here: without this, covers silently never appear, which
    // looks exactly like a broken feature. A first run with nothing configured
    // gets a hint; a run where covers were requested but cannot work gets a
    // warning.
    let (severity, cover_line) = cover_status(config.cover_mode, config.public_url.as_deref());
    match severity {
        CoverStatus::Info => println!("  {}", cover_line),
        CoverStatus::Warning => eprintln!("  {}", cover_line),
    }

    let mut rpc = Rpc::new(config.client_id.clone());
    let mut current: Option<Current> = None;
    // When Navidrome stopped reporting a track (to apply the grace period).
    let mut missing_since: Option<i64> = None;
    // Last error we printed, so a persistent failure is logged once instead of
    // once per poll (which at 1s would bury the journal in seconds).
    let mut last_error: Option<String> = None;

    loop {
        match fetch_now_playing(&client, &config) {
            Ok(Some(track)) => {
                missing_since = None;
                last_error = None;

                // The presence must be (re)sent when the track or play state
                // changes, and otherwise refreshed on a short interval so the
                // progress bar tracks Navidrome's real position (seeks, pauses).
                let track_key = format!(
                    "{}|{}|{}|{}",
                    track.track_id, track.title, track.artist, track.album
                );
                let key = format!("{}|{}", track_key, track.playing);

                let now = now_secs();
                let changed = current.as_ref().map(|c| c.key != key).unwrap_or(true);

                // Artwork only changes with the track, so it is looked up once
                // per track and cached — paying a request every second for a
                // static image would be pure waste. Pausing reuses the stored
                // one, because the cover has not changed.
                let need_cover = current
                    .as_ref()
                    .map(|c| c.track_key != track_key)
                    .unwrap_or(true);
                let cover = if need_cover {
                    match fetch_cover(&client, &config, &track.track_id) {
                        Ok(cover) => cover,
                        Err(e) => {
                            // Logged directly rather than through `report_error`:
                            // this runs once per track instead of once per
                            // second, so there is no journal flood to guard
                            // against — and it genuinely will not be retried
                            // until the next track, so "will retry" would be a
                            // lie. It must also not touch `last_error`, which
                            // belongs to the presence exchange.
                            eprintln!(
                                "Cover art lookup failed for {:?} (no image this track): {}",
                                track.title, e
                            );
                            None
                        }
                    }
                } else {
                    current.as_ref().and_then(|c| c.cover.clone())
                };

                // Resend every tick: while playing this keeps the bar in sync with
                // Navidrome; while paused the presence simply carries no
                // timestamps, so there is nothing for Discord to advance.
                let mut presence = Presence::for_track(&track, now);
                presence.cover = cover.clone();
                let activity = presence.to_activity();

                match rpc.set_activity(activity) {
                    Ok(echo) => {
                        last_error = None;

                        // Discord echoes the activity it stored. Comparing that
                        // against what we sent catches the case we could not see
                        // before: timestamps we deliberately omitted coming back
                        // would mean Discord kept a stale bar of its own accord.
                        let sent_ts = presence.start.is_some();
                        let echo_ts = echo.get("timestamps").is_some();
                        if sent_ts != echo_ts {
                            eprintln!(
                                "Discord stored timestamps={} but we sent {} — {}",
                                echo_ts,
                                if sent_ts { "some" } else { "none" },
                                if sent_ts {
                                    "the progress bar will be missing"
                                } else {
                                    "a stale progress bar will keep running"
                                }
                            );
                        }

                        // Only log real changes to keep the journal readable.
                        if changed {
                            let state = if track.playing { "playing" } else { "paused" };
                            println!(
                                "Presence set: {} — {} [{}] ({} / {}){}",
                                track.title,
                                track.artist,
                                state,
                                track.elapsed,
                                track.duration,
                                if presence.cover.is_some() {
                                    " +cover"
                                } else {
                                    ""
                                }
                            );
                        }
                        current = Some(Current {
                            key,
                            track_key,
                            cover,
                        });
                    }
                    Err(e) => {
                        report_error(&mut last_error, "presence", &e);
                        // Force a fresh attempt (and reconnect) next tick.
                        current = None;
                    }
                }
            }
            Ok(None) => {
                last_error = None;

                // Navidrome returns an empty `nowPlaying` for a moment between
                // tracks (and when playback truly stops). Hold the last track
                // briefly so the presence doesn't flicker, then clear it.
                let now = now_secs();
                let since = *missing_since.get_or_insert(now);

                if current.is_some() && now - since >= NOTHING_PLAYING_GRACE {
                    match rpc.clear_activity() {
                        Ok(()) => println!("Nothing playing — presence cleared"),
                        Err(e) => eprintln!("Failed to clear presence: {}", e),
                    }
                    current = None;
                }
            }
            Err(e) => {
                report_error(&mut last_error, "fetch", &e);
            }
        }

        std::thread::sleep(Duration::from_secs(config.poll_interval));
    }
}

/// Logs `err` unless it is identical to the one already reported.
///
/// With a one-second poll, Navidrome being briefly unreachable would otherwise
/// write the same line once a second and drown out real changes.
fn report_error(last: &mut Option<String>, what: &str, err: &str) {
    if last.as_deref() == Some(err) {
        return;
    }
    eprintln!("Failed to {} (will retry): {}", what, err);
    *last = Some(err.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response(entry: Value) -> Value {
        json!({ "subsonic-response": { "status": "ok", "nowPlaying": { "entry": entry } } })
    }

    fn entry(title: &str, state: &str) -> Value {
        json!({
            "id": "abc123",
            "title": title,
            "artist": "Artist",
            "album": "Album",
            "duration": 200,
            "positionMs": 60_000,
            "state": state,
        })
    }

    #[test]
    fn parses_playing_track() {
        let track = parse_now_playing(&response(json!([entry("Song", "playing")])))
            .unwrap()
            .unwrap();
        assert!(track.playing);
        assert_eq!(track.title, "Song");
        assert_eq!(track.elapsed, 60);
        assert_eq!(track.duration, 200);
    }

    #[test]
    fn detects_paused_track() {
        // Regression: a paused track must NOT be reported as playing.
        let track = parse_now_playing(&response(json!([entry("Song", "paused")])))
            .unwrap()
            .unwrap();
        assert!(!track.playing);
    }

    #[test]
    fn accepts_single_object_entry() {
        let track = parse_now_playing(&response(entry("Song", "playing")))
            .unwrap()
            .unwrap();
        assert!(track.playing);
        assert_eq!(track.title, "Song");
    }

    #[test]
    fn missing_state_defaults_to_playing() {
        let mut e = entry("Song", "playing");
        e.as_object_mut().unwrap().remove("state");
        let track = parse_now_playing(&response(json!([e]))).unwrap().unwrap();
        assert!(track.playing);
    }

    #[test]
    fn empty_and_missing_now_playing_yield_none() {
        assert!(parse_now_playing(&response(json!([]))).unwrap().is_none());
        let body = json!({ "subsonic-response": { "status": "ok" } });
        assert!(parse_now_playing(&body).unwrap().is_none());
    }

    #[test]
    fn non_ok_status_is_an_error() {
        let body = json!({ "subsonic-response": { "status": "failed" } });
        assert!(parse_now_playing(&body).is_err());
    }

    fn playing_track() -> NowPlaying {
        NowPlaying {
            track_id: "abc".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            elapsed: 60,
            duration: 200,
            playing: true,
        }
    }

    #[test]
    fn playing_presence_has_a_running_window() {
        let p = Presence::for_track(&playing_track(), 1_000);
        assert_eq!(p.details, "Song");
        assert_eq!(p.state, "Artist — Album");
        assert_eq!(p.start, Some(940)); // now - elapsed
        assert_eq!(p.end, Some(1140)); // start + duration
    }

    #[test]
    fn paused_presence_is_marked_and_has_no_timestamps() {
        // Discord cannot freeze a timestamp (it always renders `now - start`),
        // so the only way to stop the bar from moving on pause is to omit the
        // timestamps entirely and rely on the state marker instead.
        let mut track = playing_track();
        track.playing = false;
        let p = Presence::for_track(&track, 1_000);
        assert!(p.state.starts_with("⏸ Paused"), "state was {:?}", p.state);
        assert!(p.state.contains("Artist — Album"));
        assert_eq!(p.start, None);
        assert_eq!(p.end, None);
    }

    #[test]
    fn paused_activity_payload_omits_timestamps() {
        // Proof that the stale bar is *removed*: the serialized activity sent to
        // Discord on pause has no `timestamps` key at all. Since SET_ACTIVITY
        // assigns the whole activity, omitting the field drops the bar instead
        // of leaving Discord to keep advancing the previous one.
        let mut track = playing_track();
        track.playing = false;
        let json = serde_json::to_value(Presence::for_track(&track, 1_000).to_activity()).unwrap();
        assert!(json.get("timestamps").is_none(), "payload was {json}");
        assert_eq!(json["type"], 2); // Listening
    }

    #[test]
    fn playing_activity_payload_has_timestamps() {
        let json = serde_json::to_value(Presence::for_track(&playing_track(), 1_000).to_activity())
            .unwrap();
        assert_eq!(json["timestamps"]["start"], 940);
        assert_eq!(json["timestamps"]["end"], 1_140);
    }

    #[test]
    fn playing_without_duration_has_no_window() {
        let mut track = playing_track();
        track.duration = 0;
        let p = Presence::for_track(&track, 1_000);
        assert_eq!(p.start, None);
        assert_eq!(p.end, None);
    }

    #[test]
    fn nothing_playing_grace_is_positive() {
        const { assert!(NOTHING_PLAYING_GRACE > 0) };
    }

    #[test]
    fn missing_artist_and_album_falls_back() {
        let mut track = playing_track();
        track.artist = String::new();
        track.album = String::new();
        let p = Presence::for_track(&track, 1_000);
        assert_eq!(p.state, "Unknown artist");
    }

    #[test]
    fn default_client_id_is_the_bundled_application() {
        assert!(!DEFAULT_DISCORD_CLIENT_ID.is_empty());
        assert!(DEFAULT_DISCORD_CLIENT_ID.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn query_encoding_keeps_safe_chars_and_escapes_the_rest() {
        // A value with `&` would otherwise split the query and inject a second
        // parameter; `#` would truncate the URL at a fragment. This now guards
        // usernames, salts and track ids — the password itself never reaches a
        // query at all (see `the_password_never_reaches_a_url`).
        assert_eq!(encode_query("plain-Pass_1.0~x"), "plain-Pass_1.0~x");
        assert_eq!(encode_query("a&b=c?d#e"), "a%26b%3Dc%3Fd%23e");
        assert_eq!(encode_query("100% /"), "100%25%20%2F");
        assert_eq!(encode_query("пароль"), "%D0%BF%D0%B0%D1%80%D0%BE%D0%BB%D1%8C");
        assert_eq!(encode_query(""), "");
    }

    #[test]
    fn query_encoding_round_trips_like_urlencoding() {
        // Cross-check against the percent-decoding rules: every byte we emit
        // must be a valid `%XX` escape or an unreserved character.
        for input in ["a b", "p@ss!", "ü/ñ", "x=y&z"] {
            let encoded = encode_query(input);
            assert_eq!(encoded.len() % 2, 1, "odd length for {:?}", input);
            let mut bytes = Vec::new();
            let mut chars = encoded.chars();
            while let Some(c) = chars.next() {
                if c == '%' {
                    let hi = chars.next().expect("hex digit");
                    let lo = chars.next().expect("hex digit");
                    bytes.push(u8::from_str_radix(&format!("{}{}", hi, lo), 16).expect("hex"));
                } else {
                    bytes.push(c as u8);
                }
            }
            assert_eq!(bytes, input.as_bytes(), "mismatch for {:?}", input);
        }
    }

    #[test]
    fn position_past_the_end_does_not_invert_the_window() {
        // Navidrome can briefly report a position past the track length while
        // transitioning; that must not produce end < start.
        let mut track = playing_track();
        track.duration = 100;
        track.elapsed = 137;
        let p = Presence::for_track(&track, 1_000);
        assert_eq!(p.start, Some(900));
        assert_eq!(p.end, Some(1_000));
        assert!(p.end.unwrap() > p.start.unwrap());
    }

    #[test]
    fn negative_position_is_clamped_to_the_start() {
        let mut track = playing_track();
        track.duration = 100;
        track.elapsed = -5;
        let p = Presence::for_track(&track, 1_000);
        assert_eq!(p.start, Some(1_000));
        assert_eq!(p.end, Some(1_100));
    }

    #[test]
    fn a_repeated_error_is_logged_only_once() {
        let mut last: Option<String> = None;
        report_error(&mut last, "fetch", "HTTP error: connection refused");
        assert_eq!(last.as_deref(), Some("HTTP error: connection refused"));

        // The same failure on the next poll must stay silent.
        let before = last.clone();
        report_error(&mut last, "fetch", "HTTP error: connection refused");
        assert_eq!(last, before);

        // A *different* failure must break through.
        report_error(&mut last, "fetch", "HTTP error: timed out");
        assert_eq!(last.as_deref(), Some("HTTP error: timed out"));
    }

    #[test]
    fn http_timeout_is_bounded() {
        // An unbounded client would let a hung Navidrome freeze the loop
        // forever, and the status would silently stop updating.
        assert!(!HTTP_TIMEOUT.is_zero());
        assert!(HTTP_TIMEOUT <= Duration::from_secs(60));
    }

    /// The links attached under the status, as `(label, url)` pairs in payload
    /// order.
    fn links_in(json: &Value) -> Vec<(String, String)> {
        json["buttons"]
            .as_array()
            .expect("activity carries buttons")
            .iter()
            .map(|b| {
                (
                    b["label"].as_str().unwrap_or_default().to_string(),
                    b["url"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn both_presences_carry_the_two_links() {
        // The links are static, so a paused status keeps them — only the bar
        // and the timestamps go away.
        let mut paused = playing_track();
        paused.playing = false;

        for (label, track) in [("playing", playing_track()), ("paused", paused)] {
            let json =
                serde_json::to_value(Presence::for_track(&track, 1_000).to_activity()).unwrap();
            assert_eq!(
                links_in(&json),
                vec![
                    ("Navidrome".to_string(), NAVIDROME_LINK.to_string()),
                    ("Source".to_string(), REPOSITORY_LINK.to_string()),
                ],
                "{label} payload was {json}"
            );
        }
    }

    #[test]
    fn the_links_are_https() {
        // Discord accepts nothing but https, and a rejected activity loses the
        // entire status — not just the offending button.
        for url in [NAVIDROME_LINK, REPOSITORY_LINK] {
            assert!(url.starts_with("https://"), "{url} is not an https link");
        }
    }

    // ---- Authentication -------------------------------------------------

    fn auth_config(password: &str) -> Config {
        Config {
            base_url: "http://localhost:4533".into(),
            username: "S4ntyC1t".into(),
            password: password.into(),
            // These tests build URLs; cover mode plays no part in that, so the
            // field carries the default rather than a value that would look
            // meaningful if read in isolation.
            cover_mode: CoverMode::Off,
            public_url: None,
            client_id: DEFAULT_DISCORD_CLIENT_ID.into(),
            poll_interval: 1,
        }
    }

    #[test]
    fn the_password_never_reaches_a_url() {
        // The whole point of token auth: nothing built for the wire may carry
        // the password, nor the `p=` parameter that used to hold it.
        let config = auth_config("s3cr3t-password");
        for url in [
            subsonic_url(&config, "getNowPlaying", ""),
            subsonic_url(&config, "getAlbumInfo", "id=abc123"),
        ] {
            assert!(!url.contains("s3cr3t-password"), "password leaked: {url}");
            assert!(
                !url.contains("?p=") && !url.contains("&p="),
                "plaintext password param: {url}"
            );
            assert!(url.contains("u=S4ntyC1t"), "username missing: {url}");
            assert!(url.contains("&t="), "token missing: {url}");
            assert!(url.contains("&s="), "salt missing: {url}");
        }
    }

    #[test]
    fn the_token_is_md5_of_password_then_salt() {
        // md5("message digest") is a published RFC 1321 vector, reached here
        // with password "message " and salt "digest". It pins the algorithm
        // *and* the concatenation order: swapping to salt-then-password yields
        // 0c5b0b38d25e3867f3625344c1a8a38d, and Navidrome answers that with
        // "Wrong username or password" — an error that points nowhere near the
        // actual mistake.
        let config = auth_config("message ");
        assert!(
            auth_params_with_salt(&config, "digest")
                .contains("t=f96b697d7cb7938d525a2f31aaf161d0"),
            "digest computed over the wrong input order"
        );
    }

    #[test]
    fn salts_meet_the_subsonic_rules() {
        // "Random alphanumeric of at least 12 characters", and distinct per
        // request — a repeated salt would mean a repeated token, which is the
        // replayable thing we are rotating in the first place.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..32 {
            let salt = random_salt();
            assert!(salt.len() >= 12, "salt too short: {salt}");
            assert!(
                salt.chars().all(|c| c.is_ascii_alphanumeric()),
                "salt is not alphanumeric: {salt}"
            );
            assert!(seen.insert(salt.clone()), "salt reused: {salt}");
        }
    }

    // ---- Cover art ------------------------------------------------------

    #[test]
    fn a_configured_public_origin_replaces_the_local_one() {
        // Navidrome builds these from the Host header of *our* request, which
        // is localhost, so the origin has to be swapped before Discord ever
        // sees it. Only the origin changes — the signed path must survive
        // intact, since the JWT inside it is what authorises the fetch.
        let raw = "http://localhost:4533/share/img/eyJhbGciOiJ9.sig?size=600";
        assert_eq!(
            public_image_url(raw, Some("https://music.example.com"), CoverMode::Navidrome)
                .as_deref(),
            Some("https://music.example.com/share/img/eyJhbGciOiJ9.sig?size=600")
        );
    }

    #[test]
    fn a_url_discord_cannot_reach_is_dropped() {
        // Discord fetches presence images from its own servers. Handing it
        // something only this machine resolves buys a broken image with no
        // explanation attached, which is worse than showing no cover at all.
        for raw in [
            "http://localhost:4533/share/img/tok?size=600",
            "http://127.0.0.1:4533/share/img/tok",
            "http://10.1.2.3:4533/share/img/tok",
            "http://192.168.1.42:4533/share/img/tok",
            "http://172.20.0.7:4533/share/img/tok",
            "http://nas.local:4533/share/img/tok",
            "http://[::1]:4533/share/img/tok",
        ] {
            assert_eq!(public_image_url(raw, None, CoverMode::Navidrome), None, "{raw}");
        }
    }

    #[test]
    fn a_already_public_navidrome_url_is_left_alone() {
        // A Navidrome with a BaseUrl configured returns a usable URL straight
        // away; forcing a rewrite there would break a working setup.
        let raw = "https://music.example.com/share/img/tok?size=600";
        assert_eq!(
            public_image_url(raw, None, CoverMode::Navidrome).as_deref(),
            Some(raw)
        );
        // Even when a public origin is configured, the path is preserved.
        assert_eq!(
            public_image_url(raw, Some("https://other.example.com"), CoverMode::Navidrome)
                .as_deref(),
            Some("https://other.example.com/share/img/tok?size=600")
        );
    }

    #[test]
    fn server_mode_never_falls_back_to_navidrome() {
        // The whole reason `serve-covers` exists is that Navidrome is *not*
        // exposed. Falling back to its own URL here would hand Discord an
        // address we already know it cannot reach.
        assert_eq!(
            public_image_url(
                "https://music.example.com/share/img/tok?size=600",
                None,
                CoverMode::Server
            ),
            None
        );
        // A configured origin is still used — it is the cover server's.
        assert_eq!(
            public_image_url(
                "http://localhost:4533/share/img/tok?size=600",
                Some("https://covers.example.com"),
                CoverMode::Server
            )
            .as_deref(),
            Some("https://covers.example.com/share/img/tok?size=600")
        );
    }

    // ---- Cover mode -----------------------------------------------------

    #[test]
    fn the_default_cover_mode_is_off() {
        // No covers unless asked for: the common setup has no public endpoint,
        // and it must work out of the box without one.
        for raw in ["", "   ", "off", " OFF ", "Off"] {
            assert_eq!(CoverMode::parse(raw), Ok(CoverMode::Off), "raw: {raw:?}");
        }
    }

    #[test]
    fn the_documented_cover_modes_parse() {
        for (raw, expected) in [
            ("navidrome", CoverMode::Navidrome),
            ("NaviDrome", CoverMode::Navidrome),
            ("server", CoverMode::Server),
            ("  server  ", CoverMode::Server),
        ] {
            assert_eq!(CoverMode::parse(raw), Ok(expected), "raw: {raw:?}");
        }
    }

    #[test]
    fn an_unknown_cover_mode_is_rejected() {
        // A typo must not quietly fall back to `off`: that is precisely the
        // silent failure this variable exists to prevent.
        for raw in ["navidrom", "auto", "true", "cloudflare", "navidrome,"] {
            let err = CoverMode::parse(raw).unwrap_err();
            assert!(err.contains("off, navidrome, server"), "raw: {raw:?} -> {err}");
        }
    }

    #[test]
    fn a_first_run_without_covers_is_a_hint_not_a_failure() {
        // The single most important distinction in this function: someone who
        // has done nothing wrong must not be shown a warning on their first
        // start, or they will read it as the program being broken.
        let (severity, line) = cover_status(CoverMode::Off, None);
        assert_eq!(severity, CoverStatus::Info, "line: {line}");
        assert!(line.contains("COVER_MODE"), "line: {line}");
        assert!(!line.starts_with("Warning"), "line: {line}");
    }

    #[test]
    fn covers_that_were_asked_for_but_cannot_work_warn() {
        // Two ways to ask for covers and not get them — both must be
        // unmissable, because both leave the operator expecting an image.
        let (severity, line) = cover_status(CoverMode::Server, None);
        assert_eq!(severity, CoverStatus::Warning, "line: {line}");
        assert!(line.contains("NAVIDROME_PUBLIC_URL"), "line: {line}");

        let (severity, line) = cover_status(CoverMode::Off, Some("https://covers.example.com"));
        assert_eq!(severity, CoverStatus::Warning, "line: {line}");
        assert!(line.contains("COVER_MODE"), "line: {line}");
    }

    #[test]
    fn a_mode_that_will_work_reports_where_covers_come_from() {
        for (mode, origin, needle) in [
            (
                CoverMode::Navidrome,
                "https://music.example.com",
                "from Navidrome at https://music.example.com",
            ),
            (
                CoverMode::Server,
                "https://covers.example.com",
                "from the cover server at https://covers.example.com",
            ),
        ] {
            let (severity, line) = cover_status(mode, Some(origin));
            assert_eq!(severity, CoverStatus::Info, "line: {line}");
            assert!(line.contains(needle), "line: {line}");
        }
    }

    #[test]
    fn navidrome_mode_without_an_origin_is_not_a_warning() {
        // A Navidrome with a `BaseUrl` already returns reachable URLs, so this
        // configuration can genuinely work — crying warning here would be a
        // false alarm, and false alarms teach people to ignore real ones.
        let (severity, line) = cover_status(CoverMode::Navidrome, None);
        assert_eq!(severity, CoverStatus::Info, "line: {line}");
        assert!(line.contains("publicly reachable"), "line: {line}");
    }

    #[test]
    fn the_cover_reaches_the_activity() {
        let mut presence = Presence::for_track(&playing_track(), 1_000);
        presence.cover = Some("https://music.example.com/share/img/tok?size=600".into());

        let json = serde_json::to_value(presence.to_activity()).unwrap();

        assert_eq!(
            json["assets"]["large_image"].as_str(),
            Some("https://music.example.com/share/img/tok?size=600"),
            "payload was {json}"
        );
        // Hovering the cover shows which album it belongs to.
        assert_eq!(json["assets"]["large_text"].as_str(), Some("Album"));
    }

    #[test]
    fn without_a_cover_no_assets_are_sent() {
        // An empty or bogus `assets` object would put a broken frame under an
        // otherwise fine status.
        let json =
            serde_json::to_value(Presence::for_track(&playing_track(), 1_000).to_activity())
                .unwrap();
        assert!(json.get("assets").is_none(), "payload was {json}");
    }

    #[test]
    fn a_paused_presence_keeps_the_cover() {
        // Only the progress bar stops on pause — the artwork is static and
        // belongs there either way.
        let mut track = playing_track();
        track.playing = false;

        let mut presence = Presence::for_track(&track, 1_000);
        presence.cover = Some("https://music.example.com/share/img/tok".into());

        let json = serde_json::to_value(presence.to_activity()).unwrap();

        assert!(json.get("timestamps").is_none(), "payload was {json}");
        assert_eq!(
            json["assets"]["large_image"].as_str(),
            Some("https://music.example.com/share/img/tok")
        );
        assert!(json["state"].as_str().unwrap().starts_with("⏸ Paused"));
    }

    // ---- Cover server ---------------------------------------------------

    #[test]
    fn the_cover_server_accepts_the_urls_it_hands_out() {
        // Exactly what Navidrome produces and the bridge publishes.
        let token = "eyJhbGciOiJIUzI1NiJ9.eyJpZCI6ImFsLTRiIn0.sig";

        assert_eq!(
            cover_upstream_path(&format!("/share/img/{}?size=600", token)),
            Some(format!("/share/img/{}?size=600", token))
        );
        assert_eq!(
            cover_upstream_path(&format!("/share/img/{}", token)),
            Some(format!("/share/img/{}", token))
        );
        // Both prefixes: it is not obvious whether Funnel forwards or strips
        // the mount path, so both must work.
        assert_eq!(
            cover_upstream_path(&format!("/img/{}", token)),
            Some(format!("/share/img/{}", token))
        );
        assert_eq!(
            cover_upstream_path(&format!("/img/{}?size=1200", token)),
            Some(format!("/share/img/{}?size=1200", token))
        );
    }

    #[test]
    fn the_cover_server_refuses_everything_else() {
        // This is the whole security boundary: a request must not be able to
        // steer the upstream fetch onto another Navidrome endpoint, out of the
        // share directory, or into a header.
        for target in [
            // Not our endpoint — this is what keeps the login page and the API
            // off the public internet.
            "/",
            "/rest/getNowPlaying",
            "/login",
            "/share/img/../../rest/getNowPlaying",
            "/share/img/",
            // Escape attempts.
            "/share/img/..%2f..%2fetc/passwd",
            "/share/img/a/b",
            "/share/img/tok#frag",
            // Absolute-form target naming an origin we did not choose.
            "http://evil.example/share/img/tok",
            "https://evil.example/share/img/tok",
            // Header injection through the request line.
            "/share/img/tok\r\nX-Injected: 1",
            "/share/img/tok with space",
            // Query we would not have authorised: names another origin, so it
            // is refused (well-formed key=value pairs are forwarded — the path
            // is pinned, and upstream ignores what it does not recognise).
            "/share/img/tok?next=http://evil.example",
            "/share/img/tok?size=600/../rest/ping",
        ] {
            assert_eq!(
                cover_upstream_path(target),
                None,
                "should have been refused: {:?}",
                target
            );
        }
    }

    #[test]
    fn the_cover_server_refuses_a_bad_method() {
        // A server that proxies only artwork must not become a write surface.
        let server = CoverServer::new("http://localhost:4533".into());
        let response = String::from_utf8(server.respond("POST", "/share/img/tok")).unwrap();
        assert!(response.starts_with("HTTP/1.1 405"), "{}", &response[..40]);

        let response = String::from_utf8(server.respond("DELETE", "/share/img/tok")).unwrap();
        assert!(response.starts_with("HTTP/1.1 405"), "{}", &response[..40]);
    }

    #[test]
    fn the_cover_server_answers_404_without_touching_upstream() {
        // Nothing is listening on this port, so any upstream round-trip would
        // fail loudly rather than fast — proving the refusal happens first.
        let server = CoverServer::new("http://127.0.0.1:9".into());

        for target in ["/", "/rest/ping", "/share/img/..%2f..%2fetc/passwd"] {
            let response = String::from_utf8(server.respond("GET", target)).unwrap();
            assert!(
                response.starts_with("HTTP/1.1 404"),
                "{} -> {}",
                target,
                &response[..15.min(response.len())]
            );
        }
    }

    // ---- Cover server limits -------------------------------------------

    #[test]
    fn only_raster_images_are_republished() {
        // `image/*` is broader than it reads: `image/svg+xml` carries scripts,
        // and this server is reachable from the internet.
        for header in [
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/gif",
            // Parameters are not part of the media type.
            "image/png; charset=binary",
            "IMAGE/PNG",
        ] {
            assert!(
                allowed_cover_type(header).is_some(),
                "should have been accepted: {header}"
            );
        }

        for header in [
            "image/svg+xml",
            "image/svg+xml; charset=utf-8",
            "text/html",
            "application/json",
            "image/png/svg+xml",
            "",
        ] {
            assert_eq!(
                allowed_cover_type(header),
                None,
                "should have been refused: {header}"
            );
        }
    }

    #[test]
    fn the_cache_reports_the_bytes_it_holds() {
        // Entry count alone is not a memory bound: the whole reason the cache
        // is evicted on total size as well.
        let mut hits = HashMap::new();
        assert_eq!(total_cached_bytes(&hits), 0);

        hits.insert(
            "a".to_string(),
            CoverCacheEntry {
                bytes: Arc::new(vec![0u8; 300]),
                content_type: "image/png".into(),
            },
        );
        hits.insert(
            "b".to_string(),
            CoverCacheEntry {
                bytes: Arc::new(vec![0u8; 700]),
                content_type: "image/jpeg".into(),
            },
        );
        assert_eq!(total_cached_bytes(&hits), 1000);
    }

    #[test]
    fn a_dropped_connection_slot_frees_its_place() {
        // This counter is what bounds concurrent connections. A handler that
        // panics must still hand its slot back, or the server closes itself to
        // legitimate traffic one panic at a time — which is exactly what the
        // `Drop` impl exists for.
        let counter = Arc::new(AtomicUsize::new(0));

        counter.fetch_add(1, Ordering::Relaxed);
        {
            let _slot = ConnectionSlot(Arc::clone(&counter));
            assert_eq!(counter.load(Ordering::Relaxed), 1);
        }
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        let panicking = Arc::clone(&counter);
        let handle = std::thread::spawn(move || {
            panicking.fetch_add(1, Ordering::Relaxed);
            let _slot = ConnectionSlot(Arc::clone(&panicking));
            panic!("handler panicked");
        });
        assert!(handle.join().is_err());
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "a panicking handler leaked its connection slot"
        );
    }

    // ---- Cropping -------------------------------------------------------

    /// Encodes an image for a test, so the fixtures need no files on disk.
    fn png_of(image: &image::DynamicImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode png");
        bytes
    }

    #[test]
    fn a_wide_cover_is_cropped_to_a_centred_square() {
        // 200x100, white through the middle 60 columns (x 70..130) and black
        // at both edges. A centred crop spans x 50..150, so it must contain
        // black at its own left and right with white in between — a crop
        // anchored to either edge would not.
        let mut wide = image::RgbImage::new(200, 100);
        for (x, _, pixel) in wide.enumerate_pixels_mut() {
            let white = (70..130).contains(&x);
            *pixel = image::Rgb(if white { [255u8, 255, 255] } else { [0, 0, 0] });
        }

        let (cropped, encoded_as) =
            to_square(&png_of(&image::DynamicImage::ImageRgb8(wide))).expect("cropped");

        // Opaque source, so JPEG — the format a photo compresses to.
        assert_eq!(encoded_as, "image/jpeg");

        let out = image::load_from_memory(&cropped).expect("decode the crop");
        assert_eq!((out.width(), out.height()), (100, 100), "not a square");

        // JPEG is lossy, so the thresholds have slack.
        let rgb = out.to_rgb8();
        for (x, expect) in [(5u32, "dark"), (45, "light"), (95, "dark")] {
            let [r, g, b] = rgb.get_pixel(x, 50).0;
            match expect {
                "dark" => assert!(r < 40 && g < 40 && b < 40, "x={x} came out {r} {g} {b}"),
                _ => assert!(r > 215 && g > 215 && b > 215, "x={x} came out {r} {g} {b}"),
            }
        }
    }

    #[test]
    fn an_already_square_cover_is_left_alone() {
        // Nothing to crop, and re-encoding an image that is already what we
        // want would only lose quality for nothing.
        let square = image::DynamicImage::ImageRgb8(image::RgbImage::new(80, 80));
        assert_eq!(to_square(&png_of(&square)), None);
    }

    #[test]
    fn an_undecodable_cover_is_served_as_it_arrived() {
        // A format this build cannot decode must not become a missing cover —
        // `None` means "leave it alone", and the caller keeps the original.
        assert_eq!(to_square(b"not an image at all"), None);
        assert_eq!(to_square(b""), None);
    }

    #[test]
    fn transparency_survives_the_crop() {
        // JPEG has nowhere to put alpha and would flatten it to black, which
        // is the letterboxing this crop exists to avoid.
        let mut wide = image::RgbaImage::new(200, 100);
        for (x, _, pixel) in wide.enumerate_pixels_mut() {
            *pixel = image::Rgba([200u8, 10, 10, if (70..130).contains(&x) { 255 } else { 0 }]);
        }

        let (cropped, encoded_as) =
            to_square(&png_of(&image::DynamicImage::ImageRgba8(wide))).expect("cropped");

        assert_eq!(encoded_as, "image/png");
        let out = image::load_from_memory(&cropped).expect("decode the crop");
        assert_eq!(out.color(), image::ColorType::Rgba8, "alpha was flattened away");
        // Inside the crop, x 50..150: opaque at the middle, clear at the edges.
        assert_eq!(out.to_rgba8().get_pixel(50, 50).0[3], 255);
        assert_eq!(out.to_rgba8().get_pixel(5, 50).0[3], 0);
    }

    // ---- Host reachability ---------------------------------------------

    #[test]
    fn a_public_hostname_is_never_dropped_for_looking_like_ipv6() {
        // `fc00::/7` is unique-local, but only ever as an IPv6 literal. Read
        // as a plain hostname prefix it also matches public names, and a cover
        // dropped here is one that would have rendered.
        for origin in [
            "https://fcmusic.example.com",
            "https://fdmusic.example.com",
            "https://fc2.example.org",
            "fcmusic.example.com",
        ] {
            assert!(!is_locally_only(origin), "treated as local: {origin}");
        }
    }

    #[test]
    fn an_ipv6_unique_local_address_is_still_local() {
        // The complement of the test above: the check must not have loosened
        // into never firing.
        for origin in [
            "http://[fd00::1]:4533",
            "http://[fc00::1]",
            "fd00::1",
            "[fd12:3456::1]:4533",
        ] {
            assert!(is_locally_only(origin), "treated as public: {origin}");
        }
    }

    #[test]
    fn a_fully_qualified_localhost_is_local() {
        // `localhost.` names the same host as `localhost`; the trailing dot
        // used to slip past every check and go to Discord unresolvable.
        for origin in [
            "localhost.",
            "http://localhost.:4533",
            "LOCALHOST.",
            "http://127.0.0.1.:4533",
        ] {
            assert!(is_locally_only(origin), "treated as public: {origin}");
        }
    }
}
