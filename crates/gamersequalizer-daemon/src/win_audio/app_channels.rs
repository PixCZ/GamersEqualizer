//! Per-app channel assignments the user made by dragging apps between mixer strips.
//! They win over the built-in `CHAT_EXES` list and game detection, keyed by lowercase exe name.
//!
//! A second store remembers the exes game detection recognized (`detected_apps.json`), so a
//! game playing in the background is on Game after a restart too. Same mechanics, values always
//! Game. `channel_for` decides.
//!
//! A third store holds each app's own volume under its channel, set from the tray flyout
//! (`app_volumes.json`); the mixer multiplies it into each session's target (`volume_factor`).
//!
//! Process-wide stores: one process writes a file (the window the channels, the service the app
//! volumes), the others re-read it when its modification time changes, checked at most every
//! `RELOAD_CHECK`. Load is infallible: missing or corrupt file -> no overrides.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::output_eq::settings::write_atomic;

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

pub const APP_CHANNELS_FILE: &str = "app_channels.json";
pub const DETECTED_FILE: &str = "detected_apps.json";
pub const APP_VOLUMES_FILE: &str = "app_volumes.json";
const RELOAD_CHECK: Duration = Duration::from_secs(1);

/// Browsers stay on Media (`app_channels::channel_for`), even if game detection or a name match
/// says otherwise: YouTube plays in full quality. Lowercase exe file names.
pub const BROWSER_EXES: &[&str] = &[
  "chrome.exe",
  "chromium.exe",
  "msedge.exe",
  "firefox.exe",
  "librewolf.exe",
  "waterfox.exe",
  "floorp.exe",
  "zen.exe",
  "brave.exe",
  "opera.exe",
  "vivaldi.exe",
  "arc.exe",
  "thorium.exe",
  "iexplore.exe",
  // Privacy / Firefox forks (Tor Browser runs as firefox.exe)
  "mullvadbrowser.exe",
  "palemoon.exe",
  "basilisk.exe",
  "seamonkey.exe",
  "falkon.exe",
  "midori.exe",
  "k-meleon.exe",
  "icedragon.exe",
  "dooble.exe",
  "otter-browser.exe",
  // Chromium forks and bundled browsers (Opera GX / Air run as opera.exe)
  "whale.exe",
  "dragon.exe",
  "iron.exe",
  "slimjet.exe",
  "maxthon.exe",
  "avastbrowser.exe",
  "avgbrowser.exe",
  "ccleanerbrowser.exe",
  "nortonbrowser.exe",
  "sidekick.exe",
  "wavebox.exe",
  "360se.exe",
  "360chrome.exe",
  "qqbrowser.exe",
  "sogouexplorer.exe",
  "ucbrowser.exe",
  "aloha.exe",
  // Yandex Browser and Coc Coc: a generic name, but a wrong hit only keeps an app on Media (the default)
  "browser.exe",
  // AI browsers
  "comet.exe",
];

/// Apps on the Chat channel (ChatMix volume, chat stream EQ). Lowercase exe file names; active
/// desktop clients only. Anything missing stays on Media until the user drags it in the mixer
/// (`app_channels`).
pub const CHAT_EXES: &[&str] = &[
  // Voice chat for gamers
  "discord.exe",
  "discordptb.exe",
  "discordcanary.exe",
  "vesktop.exe",
  "ts3client_win64.exe",
  "ts3client_win32.exe",
  "teamspeak.exe",
  "mumble.exe",
  "guilded.exe",
  "ventrilo.exe",
  "teamtalk5.exe",
  "revolt.exe",
  // Messengers
  "signal.exe",
  "telegram.exe",
  "whatsapp.exe",
  "whatsapp.root.exe",
  "viber.exe",
  "element.exe",
  "wire.exe",
  "line.exe",
  "wechat.exe",
  "weixin.exe",
  "threema.exe",
  "qq.exe",
  "kakaotalk.exe",
  "pidgin.exe",
  "trillian.exe",
  "paltalk.exe",
  "jami.exe",
  "linphone.exe",
  "whatsappdesktop.exe",
  "zalo.exe",
  "camfrog video chat.exe",
  // Work / meetings
  "teams.exe",
  "ms-teams.exe",
  "slack.exe",
  "zoom.exe",
  "webex.exe",
  "ciscocollabhost.exe",
  "atmgr.exe",
  "mattermost.exe",
  "rocket.chat.exe",
  "ringcentral.exe",
  "zulip.exe",
  "lync.exe",
  "ciscojabber.exe",
  "webexhost.exe",
  "g2mvideoconference.exe",
  "ringcentralvideo.exe",
  "jitsi meet.exe",
  "g2mcomm.exe",
  "goto.exe",
  "bluejeans.exe",
  "dingtalk.exe",
  "vsee.exe",
  "alfaview.exe",
];

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

/// An app's own volume under its channel: its sessions play at channel x ChatMix x this. The
/// tray flyout sets it so that the session lands where the user dragged it
/// (`ChatMixController::set_app_heard_volume`). 100 % is the default and is not stored: the mixer
/// then behaves as before there were app volumes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppVolume {
  pub volume: f32,
  /// Fields a newer version wrote; kept through a load-save round trip.
  #[serde(flatten)]
  pub extra: BTreeMap<String, serde_json::Value>,
}

struct Store<V> {
  path: Option<PathBuf>,
  modified: Option<SystemTime>,
  last_check: Option<Instant>,
  overrides: BTreeMap<String, V>,
}

static STORE: RwLock<Store<AppChannel>> = RwLock::new(Store::new());
static DETECTED: RwLock<Store<AppChannel>> = RwLock::new(Store::new());
static VOLUMES: RwLock<Store<AppVolume>> = RwLock::new(Store::new());
/// Bumped whenever any store may have changed (`generation`).
static GENERATION: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------------------

/// Point the stores at `<base_dir>/app_channels.json` / `detected_apps.json` /
/// `app_volumes.json` and load them. Call once per process.
pub fn init(base_dir: &Path) {
  for (lock, file) in [(&STORE, APP_CHANNELS_FILE), (&DETECTED, DETECTED_FILE)] {
    lock.write().unwrap_or_else(|e| e.into_inner()).point_at(base_dir.join(file));
  }
  VOLUMES.write().unwrap_or_else(|e| e.into_inner()).point_at(base_dir.join(APP_VOLUMES_FILE));
}

/// The user's channel for this exe, if they assigned one.
pub fn get(exe: &str) -> Option<AppChannel> {
  lookup(&STORE, exe).filter(|c| *c != AppChannel::Unknown)
}

/// Assign `exe` to `channel`, or back to automatic with `None`; saved at once.
pub fn set(exe: &str, channel: Option<AppChannel>) {
  store_set(&STORE, exe, channel, true);
}

/// This exe's own volume (100 % when none).
pub fn app_volume(exe: &str) -> AppVolume {
  lookup(&VOLUMES, exe).unwrap_or_default()
}

/// Sets an app's volume. `save`: also write the file (at the end of a drag, not on every step).
pub fn set_app_volume(exe: &str, volume: AppVolume, save: bool) {
  store_set(&VOLUMES, exe, (volume != AppVolume::default()).then_some(volume), save);
}

/// What an app's session target is multiplied by (0..=1).
pub fn volume_factor(exe: &str) -> f32 {
  app_volume(exe).factor()
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

/// Moves when an assignment or a detected game may have changed (set here or the file re-read):
/// the mixer then puts the sessions on their channels at once (`apply_session_mix`).
pub fn generation() -> u32 {
  GENERATION.load(Ordering::Relaxed)
}

/// What game detection found for an exe that came to the foreground; saved when it changed.
pub fn set_detected_game(exe: &str, game: bool) {
  if (lookup(&DETECTED, exe) == Some(AppChannel::Game)) != game {
    store_set(&DETECTED, exe, game.then_some(AppChannel::Game), true);
  }
}

// ---------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------

fn lookup<V: Clone + DeserializeOwned>(lock: &RwLock<Store<V>>, exe: &str) -> Option<V> {
  refresh_if_due(lock);
  let store = lock.read().unwrap_or_else(|e| e.into_inner());
  store.overrides.get(&exe.to_ascii_lowercase()).cloned()
}

fn store_set<V: Serialize>(lock: &RwLock<Store<V>>, exe: &str, value: Option<V>, save: bool) {
  let mut store = lock.write().unwrap_or_else(|e| e.into_inner());
  let key = exe.to_ascii_lowercase();
  match value {
    Some(v) => store.overrides.insert(key, v),
    None => store.overrides.remove(&key),
  };
  GENERATION.fetch_add(1, Ordering::Relaxed);
  let Some(path) = store.path.clone().filter(|_| save) else { return };
  let saved = serde_json::to_string_pretty(&store.overrides)
    .map_err(std::io::Error::other)
    .and_then(|text| write_atomic(&path, &text));
  match saved {
    Ok(()) => store.modified = modified(&path),
    Err(e) => log::error!("[AppChannels] saving {} failed: {e}", path.display()),
  }
}

fn refresh_if_due<V: DeserializeOwned>(lock: &RwLock<Store<V>>) {
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

impl Default for AppVolume {
  fn default() -> Self {
    Self { volume: 1.0, extra: BTreeMap::new() }
  }
}

impl AppVolume {
  pub fn factor(&self) -> f32 {
    if self.volume.is_finite() { self.volume.clamp(0.0, 1.0) } else { 1.0 }
  }
}

impl<V> Store<V> {
  const fn new() -> Self {
    Self { path: None, modified: None, last_check: None, overrides: BTreeMap::new() }
  }
}

impl<V: DeserializeOwned> Store<V> {
  fn point_at(&mut self, path: PathBuf) {
    self.path = Some(path);
    self.reload();
  }

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
    GENERATION.fetch_add(1, Ordering::Relaxed);
  }
}

fn parse<V: DeserializeOwned>(text: &str) -> serde_json::Result<BTreeMap<String, V>> {
  let map: BTreeMap<String, V> = serde_json::from_str(text)?;
  Ok(map.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect())
}

fn modified(path: &Path) -> Option<SystemTime> {
  std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashSet;

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
    let map: BTreeMap<String, AppChannel> =
      parse(r#"{"Spotify.exe":"chat","x.exe":"game","new.exe":"voice"}"#).unwrap();
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
    assert_eq!(parse::<AppChannel>(&text).unwrap().get("spotify.exe"), Some(&AppChannel::Chat));
    set("spotify.exe", None);
    assert_eq!(get("Spotify.exe"), None);

    // App volumes: the default is not stored, a drag step is not saved until its end.
    assert_eq!(volume_factor("game.exe"), 1.0);
    let half = AppVolume { volume: 0.5, ..AppVolume::default() };
    set_app_volume("Game.exe", half.clone(), false);
    assert_eq!(volume_factor("game.exe"), 0.5);
    assert!(!dir.join(APP_VOLUMES_FILE).exists());
    set_app_volume("game.exe", half, true);
    assert_eq!(volume_factor("GAME.exe"), 0.5);
    let text = std::fs::read_to_string(dir.join(APP_VOLUMES_FILE)).unwrap();
    let saved = parse::<AppVolume>(&text).unwrap();
    assert_eq!(saved.get("game.exe").map(|v| v.volume), Some(0.5));
    set_app_volume("game.exe", AppVolume::default(), true);
    assert_eq!(std::fs::read_to_string(dir.join(APP_VOLUMES_FILE)).unwrap().trim(), "{}");
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn app_volumes_keep_unknown_fields_and_default_what_is_missing() {
    let map: BTreeMap<String, AppVolume> = parse(r#"{"a.exe":{"volume":0.3,"curve":"log"},"b.exe":{}}"#).unwrap();
    let a = map.get("a.exe").unwrap();
    assert_eq!(a.volume, 0.3);
    assert!(serde_json::to_string(a).unwrap().contains(r#""curve":"log""#));
    assert_eq!(map.get("b.exe"), Some(&AppVolume::default()));
    // Out of range or broken values never reach Windows as such.
    assert_eq!(AppVolume { volume: 7.0, ..AppVolume::default() }.factor(), 1.0);
    assert_eq!(AppVolume { volume: f32::NAN, ..AppVolume::default() }.factor(), 1.0);
  }

  #[test]
  fn app_lists_are_lowercase_exes_without_duplicates_or_overlap() {
    let mut seen = HashSet::new();
    for exe in CHAT_EXES.iter().chain(BROWSER_EXES) {
      assert!(exe.ends_with(".exe") && *exe == exe.to_ascii_lowercase(), "{exe}: lookups are lowercase");
      assert!(seen.insert(*exe), "{exe} is listed twice (or on both channels)");
    }
  }
}
