//! Per-app channel assignments the user made by dragging apps between mixer strips.
//! They win over the built-in `CHAT_EXES` list and game detection, keyed by lowercase exe name.
//!
//! A second store remembers the exes game detection recognized (`detected_apps.json`), so a
//! game playing in the background is on Game after a restart too. Same mechanics, values always
//! Game. `channel_for` decides.
//!
//! Process-wide store: the UI process writes the file, the daemon (volume mix + stream
//! router thread) re-reads it when its modification time changes, checked at most every
//! `RELOAD_CHECK`. Load is infallible: missing or corrupt file -> no overrides.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use super::chatmix::{BROWSER_EXES, CHAT_EXES};
use crate::output_eq::settings::write_atomic;

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

pub const APP_CHANNELS_FILE: &str = "app_channels.json";
pub const DETECTED_FILE: &str = "detected_apps.json";
const RELOAD_CHECK: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------

/// Channel an app can be dragged to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppChannel {
  Game,
  Chat,
  Media,
  /// Written by a newer version; ignored (the app falls back to the lists).
  #[serde(other)]
  Unknown,
}

#[derive(Default)]
struct Store {
  path: Option<PathBuf>,
  modified: Option<SystemTime>,
  last_check: Option<Instant>,
  overrides: BTreeMap<String, AppChannel>,
}

static STORE: RwLock<Store> =
  RwLock::new(Store { path: None, modified: None, last_check: None, overrides: BTreeMap::new() });
static DETECTED: RwLock<Store> =
  RwLock::new(Store { path: None, modified: None, last_check: None, overrides: BTreeMap::new() });

// ---------------------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------------------

/// Point the stores at `<base_dir>/app_channels.json` / `detected_apps.json` and load them.
/// Call once per process.
pub fn init(base_dir: &Path) {
  for (lock, file) in [(&STORE, APP_CHANNELS_FILE), (&DETECTED, DETECTED_FILE)] {
    let mut store = lock.write().unwrap_or_else(|e| e.into_inner());
    store.path = Some(base_dir.join(file));
    store.reload();
  }
}

/// The user's channel for this exe, if they assigned one.
pub fn get(exe: &str) -> Option<AppChannel> {
  lookup(&STORE, exe).filter(|c| *c != AppChannel::Unknown)
}

/// Assign `exe` to `channel`, or back to automatic with `None`; saved at once.
pub fn set(exe: &str, channel: Option<AppChannel>) {
  store_set(&STORE, exe, channel);
}

/// The channel an app's sound goes to (mixer volumes and stream EQ use this one rule):
/// 1. the user's drag in the mixer;
/// 2. browsers: Media, always (full-quality music; a Meet call must not move YouTube to Chat);
/// 3. the chat list;
/// 4. a game game detection recognized (even with its own voice chat);
/// 5. everything else: Media, the safe side (full quality; a missing chat app is dragged once).
///    No guessing from microphone use: a recorder or voice changer would land on the chat EQ.
pub fn channel_for(exe: &str) -> AppChannel {
  if let Some(c) = get(exe) {
    return c;
  }
  let exe = exe.to_ascii_lowercase();
  if BROWSER_EXES.contains(&exe.as_str()) {
    return AppChannel::Media;
  }
  if CHAT_EXES.contains(&exe.as_str()) {
    return AppChannel::Chat;
  }
  if lookup(&DETECTED, &exe) == Some(AppChannel::Game) {
    return AppChannel::Game;
  }
  AppChannel::Media
}

/// What game detection found for an exe that came to the foreground; saved when it changed.
pub fn set_detected_game(exe: &str, game: bool) {
  if (lookup(&DETECTED, exe) == Some(AppChannel::Game)) != game {
    store_set(&DETECTED, exe, game.then_some(AppChannel::Game));
  }
}

// ---------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------

fn lookup(lock: &RwLock<Store>, exe: &str) -> Option<AppChannel> {
  refresh_if_due(lock);
  let store = lock.read().unwrap_or_else(|e| e.into_inner());
  store.overrides.get(&exe.to_ascii_lowercase()).copied()
}

fn store_set(lock: &RwLock<Store>, exe: &str, channel: Option<AppChannel>) {
  let mut store = lock.write().unwrap_or_else(|e| e.into_inner());
  let key = exe.to_ascii_lowercase();
  match channel {
    Some(c) => store.overrides.insert(key, c),
    None => store.overrides.remove(&key),
  };
  let Some(path) = store.path.clone() else { return };
  let saved = serde_json::to_string_pretty(&store.overrides)
    .map_err(std::io::Error::other)
    .and_then(|text| write_atomic(&path, &text));
  match saved {
    Ok(()) => store.modified = modified(&path),
    Err(e) => log::error!("[AppChannels] saving {} failed: {e}", path.display()),
  }
}

fn refresh_if_due(lock: &RwLock<Store>) {
  let due = {
    let store = lock.read().unwrap_or_else(|e| e.into_inner());
    store.path.is_some() && store.last_check.is_none_or(|t| t.elapsed() >= RELOAD_CHECK)
  };
  if due {
    let mut store = lock.write().unwrap_or_else(|e| e.into_inner());
    store.last_check = Some(Instant::now());
    let changed = store.path.as_deref().map(modified) != Some(store.modified);
    if changed {
      store.reload();
    }
  }
}

impl Store {
  fn reload(&mut self) {
    let Some(path) = self.path.clone() else { return };
    self.modified = modified(&path);
    self.overrides = match std::fs::read_to_string(&path) {
      Ok(text) => parse(&text).unwrap_or_else(|e| {
        log::warn!("[AppChannels] {} is corrupt ({e}), ignoring it", path.display());
        crate::output_eq::settings::keep_corrupt(&path);
        BTreeMap::new()
      }),
      Err(_) => BTreeMap::new(),
    };
  }
}

fn parse(text: &str) -> serde_json::Result<BTreeMap<String, AppChannel>> {
  let map: BTreeMap<String, AppChannel> = serde_json::from_str(text)?;
  Ok(map.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect())
}

fn modified(path: &Path) -> Option<SystemTime> {
  std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn browsers_stay_media_chat_from_the_list_games_from_detection() {
    assert_eq!(channel_for("Chrome.exe"), AppChannel::Media);
    assert_eq!(channel_for("discord.exe"), AppChannel::Chat);
    assert_eq!(channel_for("unknown-tool.exe"), AppChannel::Media);
    set_detected_game("SomeGame.exe", true);
    assert_eq!(channel_for("somegame.exe"), AppChannel::Game);
    set_detected_game("somegame.exe", false);
    assert_eq!(channel_for("somegame.exe"), AppChannel::Media);
  }

  #[test]
  fn parse_lowercases_keys_and_keeps_unknown_channels_out_of_get() {
    let map = parse(r#"{"Spotify.exe":"chat","x.exe":"game","new.exe":"voice"}"#).unwrap();
    assert_eq!(map.get("spotify.exe"), Some(&AppChannel::Chat));
    assert_eq!(map.get("x.exe"), Some(&AppChannel::Game));
    assert_eq!(map.get("new.exe"), Some(&AppChannel::Unknown));
  }

  #[test]
  fn set_saves_and_another_reader_sees_it() {
    let dir = std::env::temp_dir().join(format!("ge_app_channels_{}", std::process::id()));
    init(&dir);
    set("Spotify.exe", Some(AppChannel::Chat));
    assert_eq!(get("spotify.exe"), Some(AppChannel::Chat));
    let text = std::fs::read_to_string(dir.join(APP_CHANNELS_FILE)).unwrap();
    assert_eq!(parse(&text).unwrap().get("spotify.exe"), Some(&AppChannel::Chat));
    set("spotify.exe", None);
    assert_eq!(get("Spotify.exe"), None);
    let _ = std::fs::remove_dir_all(&dir);
  }
}
