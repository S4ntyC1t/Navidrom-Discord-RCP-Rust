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
//! | `POLL_INTERVAL`      | no       | `3` (seconds)           |

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use discord_rich_presence::activity::{Activity, ActivityType, StatusDisplayType, Timestamps};
use discord_rich_presence::{DiscordIpc, DiscordIpcClient};
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
  POLL_INTERVAL       Poll interval in seconds (default 3)";

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
                .unwrap_or(3),
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
    /// Builds the presence for a track. A paused track gets an explicit
    /// "⏸ Paused" marker and no timestamps (so Discord does not show a moving
    /// progress bar); a playing track gets a `start`/`end` window that Discord
    /// advances itself.
    fn for_track(track: &NowPlaying, now: i64) -> Self {
        let credits = format!("{} — {}", track.artist, track.album).trim_end_matches(" — ").to_string();
        let credits = if credits.is_empty() {
            "Unknown artist".to_string()
        } else {
            credits
        };

        if track.playing {
            let (start, end) = if track.duration > 0 {
                let start = now - track.elapsed;
                (Some(start), Some(start + track.duration))
            } else {
                // Playing, but length unknown: no progress bar possible.
                (None, None)
            };
            Self {
                details: track.title.clone(),
                state: credits,
                start,
                end,
            }
        } else {
            Self {
                details: track.title.clone(),
                state: format!("⏸ Paused · {}", credits),
                start: None,
                end: None,
            }
        }
    }
}

/// Thin wrapper around the Discord IPC client that reconnects on demand.
struct Rpc {
    client_id: String,
    client: Option<DiscordIpcClient>,
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
        let mut client = DiscordIpcClient::new(&self.client_id);
        client
            .connect()
            .map_err(|e| format!("Discord IPC connect failed: {}", e))?;
        self.client = Some(client);
        Ok(())
    }

    fn set_activity(&mut self, activity: Activity) -> Result<(), String> {
        self.ensure_connected()?;
        let result = {
            let client = self.client.as_mut().expect("client is connected");
            client.set_activity(activity)
        };
        if let Err(e) = result {
            // Drop the broken client so the next call reconnects.
            self.client = None;
            return Err(format!("set_activity failed: {}", e));
        }
        Ok(())
    }

    fn clear_activity(&mut self) -> Result<(), String> {
        if self.client.is_none() {
            return Ok(());
        }
        let result = {
            let client = self.client.as_mut().expect("client is connected");
            client.clear_activity()
        };
        if let Err(e) = result {
            self.client = None;
            return Err(format!("clear_activity failed: {}", e));
        }
        Ok(())
    }
}

/// The presence we last pushed, so periodic re-asserts keep the same content.
struct Current {
    key: String,
    sent_at: i64,
}

/// How often to re-assert an unchanged presence (Discord can drop it).
const REASSERT_AFTER: i64 = 30;

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

    loop {
        match fetch_now_playing(&config) {
            Ok(Some(track)) => {
                // Presence only needs to change when the track (or play state)
                // changes; Discord ticks the progress bar on its own.
                let key = format!(
                    "{}|{}|{}|{}|{}",
                    track.track_id, track.title, track.artist, track.album, track.playing
                );

                let stale = current
                    .as_ref()
                    .map(|c| now_secs() - c.sent_at >= REASSERT_AFTER)
                    .unwrap_or(false);
                let changed = current.as_ref().map(|c| c.key != key).unwrap_or(true);

                if changed || stale {
                    let now = now_secs();
                    let presence = Presence::for_track(&track, now);

                    let mut activity = Activity::new()
                        .activity_type(ActivityType::Listening)
                        .status_display_type(StatusDisplayType::Details)
                        .details(presence.details.clone())
                        .state(presence.state.clone());

                    if let (Some(start), Some(end)) = (presence.start, presence.end) {
                        activity = activity.timestamps(Timestamps::new().start(start).end(end));
                    }

                    match rpc.set_activity(activity) {
                        Ok(()) => {
                            let state = if track.playing { "playing" } else { "paused" };
                            println!(
                                "Presence set: {} — {} [{}] ({} / {})",
                                track.title, track.artist, state, track.elapsed, track.duration
                            );
                            current = Some(Current {
                                key,
                                sent_at: now,
                            });
                        }
                        Err(e) => {
                            eprintln!("Failed to update presence: {}", e);
                            // Force a fresh attempt (and reconnect) next tick.
                            current = None;
                        }
                    }
                }
            }
            Ok(None) => {
                if current.is_some() {
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
        // Regression: a paused track must be visually distinguishable from a
        // playing one (explicit marker) and must not show a moving progress bar.
        let mut track = playing_track();
        track.playing = false;
        let p = Presence::for_track(&track, 1_000);
        assert!(p.state.starts_with("⏸ Paused"), "state was {:?}", p.state);
        assert!(p.state.contains("Artist — Album"));
        assert_eq!(p.start, None);
        assert_eq!(p.end, None);
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