//! Navidrome -> Discord Rich Presence bridge.
//!
//! Polls Navidrome's Subsonic API for the track that is currently playing and
//! mirrors it into Discord Rich Presence over the local Discord IPC socket.
//!
//! Configuration is provided through environment variables:
//!
//! | Variable             | Required | Default                 |
//! |----------------------|----------|-------------------------|
//! | `NAVIDROME_USER`     | yes      | —                       |
//! | `NAVIDROME_PASSWORD` | yes      | —                       |
//! | `NAVIDROME_URL`      | no       | `http://localhost:4533` |
//! | `DISCORD_CLIENT_ID`  | no       | built-in application    |
//! | `POLL_INTERVAL`      | no       | `1` (second)            |

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use discord_rich_presence::activity::{Activity, ActivityType, StatusDisplayType, Timestamps};
use reqwest::blocking::get;
use serde_json::Value;

/// Discord Application ID bundled with the project. It is a public identifier
/// (not a secret — there is no client secret involved), so it is safe to ship
/// and lets the bridge work out of the box. Override it with `DISCORD_CLIENT_ID`
/// if you want the status to show under your own application name/icon.
const DEFAULT_DISCORD_CLIENT_ID: &str = "1559471162419847198";

const USAGE: &str = "\
Required environment variables:
  NAVIDROME_USER      Your Navidrome username
  NAVIDROME_PASSWORD  Your Navidrome password

Optional:
  NAVIDROME_URL       Navidrome base URL       (default http://localhost:4533)
  DISCORD_CLIENT_ID   Override the built-in Discord Application ID
  POLL_INTERVAL       Poll interval in seconds (default 1)";

/// Everything the bridge needs to run, read once at startup.
struct Config {
    base_url: String,
    username: String,
    password: String,
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

        Ok(Self {
            base_url: std::env::var("NAVIDROME_URL")
                .unwrap_or_else(|_| "http://localhost:4533".to_string()),
            username: required("NAVIDROME_USER")?,
            password: required("NAVIDROME_PASSWORD")?,
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

/// Returns the currently playing track, `None` when nothing is playing,
/// or an error string when the request itself failed.
fn fetch_now_playing(config: &Config) -> Result<Option<NowPlaying>, String> {
    let url = format!(
        "{}/rest/getNowPlaying?u={}&p={}&v=1.16.1&c=navidrome-discord-rpc&f=json",
        config.base_url, config.username, config.password
    );

    let resp = get(&url).map_err(|e| format!("HTTP error: {}", e))?;
    let text = resp.text().map_err(|e| format!("HTTP body error: {}", e))?;
    let body: Value =
        serde_json::from_str(&text).map_err(|e| format!("JSON parse error: {}", e))?;

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

/// The presence fields we compute for a track, kept separate from the Discord
/// `Activity` type so the logic is unit-testable.
#[derive(Debug, Clone, PartialEq)]
struct Presence {
    details: String,
    state: String,
    start: Option<i64>,
    end: Option<i64>,
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
                let start = now - track.elapsed;
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
    key: String,
}

/// How long to keep showing the last track after Navidrome reports nothing.
/// Navidrome briefly returns an empty `nowPlaying` between tracks; clearing the
/// status immediately made the presence flicker, so we wait before clearing.
const NOTHING_PLAYING_GRACE: i64 = 12;

fn main() {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error: {}", e);
            eprintln!("\n{}", USAGE);
            std::process::exit(2);
        }
    };

    println!("Navidrome -> Discord Rich Presence starting");
    println!("  Navidrome: {}", config.base_url);
    println!("  User: {}", config.username);
    println!("  Poll interval: {}s", config.poll_interval);
    println!("  Discord client_id: {}", config.client_id);

    let mut rpc = Rpc::new(config.client_id.clone());
    let mut current: Option<Current> = None;
    // When Navidrome stopped reporting a track (to apply the grace period).
    let mut missing_since: Option<i64> = None;

    loop {
        match fetch_now_playing(&config) {
            Ok(Some(track)) => {
                missing_since = None;

                // The presence must be (re)sent when the track or play state
                // changes, and otherwise refreshed on a short interval so the
                // progress bar tracks Navidrome's real position (seeks, pauses).
                let key = format!(
                    "{}|{}|{}|{}|{}",
                    track.track_id, track.title, track.artist, track.album, track.playing
                );

                let now = now_secs();
                let changed = current.as_ref().map(|c| c.key != key).unwrap_or(true);

                // Resend every tick: while playing this keeps the bar in sync with
                // Navidrome; while paused the presence simply carries no
                // timestamps, so there is nothing for Discord to advance.
                let presence = Presence::for_track(&track, now);
                let activity = presence.to_activity();

                match rpc.set_activity(activity) {
                    Ok(echo) => {
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
                                "Presence set: {} — {} [{}] ({} / {})",
                                track.title, track.artist, state, track.elapsed, track.duration
                            );
                        }
                        current = Some(Current { key });
                    }
                    Err(e) => {
                        eprintln!("Failed to update presence: {}", e);
                        // Force a fresh attempt (and reconnect) next tick.
                        current = None;
                    }
                }
            }
            Ok(None) => {
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
                eprintln!("Failed to fetch now playing info: {}", e);
            }
        }

        std::thread::sleep(Duration::from_secs(config.poll_interval));
    }
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
}