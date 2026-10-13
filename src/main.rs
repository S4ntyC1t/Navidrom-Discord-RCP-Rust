//! Navidrome -> Discord Rich Presence bridge.
//!
//! Polls Navidrome's Subsonic API for the track that is currently playing and
//! mirrors it into Discord Rich Presence over the local Discord IPC socket.
//!
//! Configuration is provided entirely through environment variables:
//!
//! | Variable             | Required | Default                 |
//! |----------------------|----------|-------------------------|
//! | `DISCORD_CLIENT_ID`  | yes      | —                       |
//! | `NAVIDROME_USER`     | yes      | —                       |
//! | `NAVIDROME_PASSWORD` | yes      | —                       |
//! | `NAVIDROME_URL`      | no       | `http://localhost:4533` |
//! | `POLL_INTERVAL`      | no       | `3` (seconds)           |

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use discord_rich_presence::activity::{Activity, ActivityType, StatusDisplayType, Timestamps};
use discord_rich_presence::{DiscordIpc, DiscordIpcClient};
use reqwest::blocking::get;
use serde_json::Value;

const USAGE: &str = "\
Required environment variables:
  DISCORD_CLIENT_ID   Application ID from https://discord.com/developers/applications
  NAVIDROME_USER      Your Navidrome username
  NAVIDROME_PASSWORD  Your Navidrome password

Optional:
  NAVIDROME_URL       Navidrome base URL       (default http://localhost:4533)
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
            client_id: required("DISCORD_CLIENT_ID")?,
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

    let status = body["subsonic-response"]["status"].as_str().unwrap_or("");
    if status != "ok" {
        return Err(format!("Navidrome API status not ok: {}", status));
    }

    let np = &body["subsonic-response"]["nowPlaying"];
    if np.is_null() {
        return Ok(None);
    }

    // Navidrome reports the play state; older versions may omit it, in which
    // case we optimistically assume the track is playing.
    let playing = np["entry"]["state"]
        .as_str()
        .map(|s| s.eq_ignore_ascii_case("playing"))
        .unwrap_or(true);

    let entry = &np["entry"];
    if let Some(first) = entry.as_array().and_then(|a| a.first()) {
        if !first["title"].is_null() {
            return Ok(Some(track_from(first, playing)));
        }
    } else if !entry["title"].is_null() {
        return Ok(Some(track_from(entry, playing)));
    }

    Ok(None)
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
                    let (start, end) = if track.playing && track.duration > 0 {
                        let start = now - track.elapsed;
                        (Some(start), Some(start + track.duration))
                    } else {
                        (None, None)
                    };

                    let mut activity = Activity::new()
                        .activity_type(ActivityType::Listening)
                        .status_display_type(StatusDisplayType::Details)
                        .details(track.title.clone())
                        .state(format!("{} — {}", track.artist, track.album));

                    if let (Some(start), Some(end)) = (start, end) {
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