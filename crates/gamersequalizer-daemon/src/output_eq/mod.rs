//! Output EQ = headphone correction (per output device, e.g. from AutoEQ) followed by the game
//! profile (switched by game detection), sent as one band list to the output APO through
//! shared memory. The master switch also bypasses the microphone chain.
//!
//! Chat / media channels: the output APO runs after the mix, so Discord and the browser get
//! headphone + game there too. The stream APO (one instance per app stream, before the mix)
//! runs `channel profile + inverted game profile` on streams the daemon paired with a chat or
//! media app, so those end up with headphone + channel profile. Their preamp is the
//! difference between their own auto-preamp and the output's.
//!
//! Two processes run this engine (daemon and GUI). They stay consistent because:
//! - settings and profiles live in files; the writer bumps `out_settings_version` and the
//!   other process reloads;
//! - only the *detection owner* (the daemon, or the GUI when no daemon runs) decides the
//!   active game and publishes it in shared memory; everyone follows that value.
//!
//! Both then compute the same bands from the same inputs, so either may publish them.

pub mod autoeq;
pub mod game_match;
pub mod game_presets;
pub mod launchers;
pub mod settings;

use crate::dsp_apo::{compute_combined_engine, CombinedEngineResult};
use crate::sonar_importer::ProfileStorage;
use crate::win_audio::game_detector::{is_process_running, GameForegroundEvent};
use game_match::{GameFacts, GameMatch};
use gamersequalizer_common::biquad::{MAX_OUT_EQ_BANDS, MAX_STREAM_EQ_BANDS};
use gamersequalizer_common::profile::{expand_slopes, invert_bands, BiquadBand, ParametricProfile, VadType};
use gamersequalizer_common::shm::{ActiveGame, ShmHeader};
use gamersequalizer_common::streams::StreamBus;
use log::{info, warn};
use settings::OutputEqSettings;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// How often the detection owner checks that the active game is still running.
const ALIVE_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const RECENT_APPS: usize = 12;
/// Our own windows never count as a game or a recent app.
const OWN_EXE: &str = "gamersequalizer.exe";
/// Loudness matching: the average gain in this range (where hearing is most sensitive) is
/// compensated instead of the highest peak.
const LOUDNESS_BAND_HZ: (f32, f32) = (200.0, 6000.0);
/// Loudness matching lets EQ peaks go at most this far above 0 dBFS (the APO's soft limiter
/// handles them); a stronger boost is still partly compensated.
const MATCH_MAX_PEAK_DB: f32 = 3.0;
const PREAMP_RANGE_DB: (f32, f32) = (-30.0, 12.0);
/// Deleted profiles are moved here (recoverable), not removed.
const DELETED_DIR: &str = "_deleted";

/// A foreground app seen recently, for "assign this app to a profile".
#[derive(Debug, Clone, PartialEq)]
pub struct RecentApp {
  pub exe_name: String,
  pub label: String,
  /// How it was classified when it came to the foreground.
  pub matched: Option<GameMatch>,
}

pub struct OutputEq {
  storage: ProfileStorage,
  settings_path: PathBuf,
  pub settings: OutputEqSettings,
  pub headphone_profiles: Vec<ParametricProfile>,
  pub game_profiles: Vec<ParametricProfile>,
  pub chat_profiles: Vec<ParametricProfile>,
  pub media_profiles: Vec<ParametricProfile>,
  /// Output endpoint the headphone profile is chosen for (the Master output).
  pub output_device: Option<String>,
  pub active_game: Option<ActiveGame>,
  pub recent_apps: VecDeque<RecentApp>,
  /// Curves for the UI (headphone, game, combined) and the auto-preamp.
  pub result: CombinedEngineResult,
  /// Volume change actually applied (automatic preamp + profile gains), dB.
  pub preamp_db: f32,
  /// Highest level of the combined curve after the preamp; > 0 = the limiter works.
  pub peak_after_db: f32,
  /// (pid, full path) of the active game; only the detection owner tracks it.
  tracked: Option<(u32, String)>,
  /// This process decides the active game (set by `tick` / `on_foreground`).
  is_owner: bool,
  last_alive_check: Instant,
  facts_cache: HashMap<String, GameFacts>,
  seen_settings_version: Option<u32>,
}

impl OutputEq {
  pub fn new(storage: ProfileStorage) -> Self {
    let settings_path = storage.base_dir().join(settings::SETTINGS_FILE);
    let settings = OutputEqSettings::load_or_default(&settings_path);
    let mut engine = Self {
      headphone_profiles: storage.load_headphone_profiles(),
      game_profiles: storage.load_profiles_for_vad(VadType::Game),
      chat_profiles: storage.load_profiles_for_vad(VadType::Chat),
      media_profiles: storage.load_profiles_for_vad(VadType::Media),
      storage,
      settings_path,
      settings,
      output_device: None,
      active_game: None,
      recent_apps: VecDeque::with_capacity(RECENT_APPS),
      result: compute_combined_engine(&[], &[], 48_000.0),
      preamp_db: 0.0,
      peak_after_db: 0.0,
      tracked: None,
      is_owner: false,
      last_alive_check: Instant::now(),
      facts_cache: HashMap::new(),
      seen_settings_version: None,
    };
    engine.recompute();
    engine
  }

  // --- what is active ------------------------------------------------------------------

  pub fn headphone_profile_name(&self) -> Option<&str> {
    let device = self.output_device.as_ref()?;
    self.settings.headphone_by_device.get(device).map(String::as_str)
  }

  pub fn headphone_profile(&self) -> Option<&ParametricProfile> {
    let name = self.headphone_profile_name()?;
    self.headphone_profiles.iter().find(|p| p.name == name)
  }

  /// Detected game's profile, else the fallback.
  pub fn game_profile_name(&self) -> Option<&str> {
    match self.active_game.as_ref() {
      Some(game) if !game.profile.is_empty() => Some(game.profile.as_str()),
      _ => self.settings.fallback_game_profile.as_deref(),
    }
  }

  pub fn game_profile(&self) -> Option<&ParametricProfile> {
    let name = self.game_profile_name()?;
    self.game_profiles.iter().find(|p| p.name == name)
  }

  pub fn game_profile_mut(&mut self) -> Option<&mut ParametricProfile> {
    let name = self.game_profile_name()?.to_string();
    self.game_profiles.iter_mut().find(|p| p.name == name)
  }

  pub fn headphone_profile_mut(&mut self) -> Option<&mut ParametricProfile> {
    let name = self.headphone_profile_name()?.to_string();
    self.headphone_profiles.iter_mut().find(|p| p.name == name)
  }

  /// Chat / media profiles to pick from; empty for other buses.
  pub fn bus_profiles(&self, bus: StreamBus) -> &[ParametricProfile] {
    match bus {
      StreamBus::Chat => &self.chat_profiles,
      StreamBus::Media => &self.media_profiles,
      _ => &[],
    }
  }

  pub fn bus_enabled(&self, bus: StreamBus) -> bool {
    match bus {
      StreamBus::Chat => self.settings.chat_enabled,
      StreamBus::Media => self.settings.media_enabled,
      _ => false,
    }
  }

  pub fn bus_profile_name(&self, bus: StreamBus) -> Option<&str> {
    match bus {
      StreamBus::Chat => self.settings.chat_profile.as_deref(),
      StreamBus::Media => self.settings.media_profile.as_deref(),
      _ => None,
    }
  }

  pub fn bus_profile(&self, bus: StreamBus) -> Option<&ParametricProfile> {
    let name = self.bus_profile_name(bus)?;
    self.bus_profiles(bus).iter().find(|p| p.name == name)
  }

  pub fn bus_profile_mut(&mut self, bus: StreamBus) -> Option<&mut ParametricProfile> {
    let name = self.bus_profile_name(bus)?.to_string();
    let list = match bus {
      StreamBus::Chat => &mut self.chat_profiles,
      StreamBus::Media => &mut self.media_profiles,
      _ => return None,
    };
    list.iter_mut().find(|p| p.name == name)
  }

  /// Headphone + channel profile curves, volume change and peak after it, i.e. what a
  /// paired chat/media app sounds like.
  pub fn bus_result(&self, bus: StreamBus) -> (CombinedEngineResult, f32, f32) {
    let (hp, own) = self.bus_layers(bus);
    self.volume_change(hp, own)
  }

  // --- publishing --------------------------------------------------------------------------

  /// Recompute curves and publish the combined EQ + master switch.
  pub fn apply(&mut self, shm: Option<&ShmHeader>) {
    self.recompute();
    let Some(h) = shm else {
      return;
    };
    let s = &self.settings;
    let enabled_stages = |p: Option<&ParametricProfile>| -> Vec<BiquadBand> {
      expand_slopes(&p.map(|p| p.bands.clone()).unwrap_or_default()).into_iter().filter(|b| b.enabled).collect()
    };
    let hp = enabled_stages(self.headphone_profile().filter(|_| s.enabled && s.headphone_enabled));
    let game = enabled_stages(self.game_profile().filter(|_| s.enabled && s.game_enabled));
    // Stages as the output APO runs them: headphone first, then game, up to the slot count.
    let mut bands: Vec<BiquadBand> = hp.iter().chain(&game).cloned().collect();
    if bands.len() > MAX_OUT_EQ_BANDS {
      warn!("[OutputEq] {} filter stages, only the first {MAX_OUT_EQ_BANDS} are applied", bands.len());
      bands.truncate(MAX_OUT_EQ_BANDS);
    }
    let preamp = if s.enabled { self.preamp_db } else { 0.0 };
    h.master_bypass.store(!s.enabled, Ordering::Relaxed);
    h.write_out_eq(&bands, preamp);

    // The stream EQ undoes exactly the game stages the output APO got.
    let game_applied = bands.get(hp.len()..).unwrap_or_default();
    for (bus, slot) in StreamBus::WITH_EQ.iter().zip(&h.stream_eq) {
      let (stages, bus_preamp) = if s.enabled { self.stream_eq(*bus, game_applied, preamp) } else { (Vec::new(), 0.0) };
      slot.write(&stages, bus_preamp);
    }
  }

  /// Select the chat / media profile (None = flat).
  pub fn select_bus_profile(&mut self, bus: StreamBus, name: Option<String>, shm: Option<&ShmHeader>) {
    match bus {
      StreamBus::Chat => self.settings.chat_profile = name,
      StreamBus::Media => self.settings.media_profile = name,
      _ => return,
    }
    self.save_settings(shm);
  }

  pub fn save_bus_profile(&mut self, bus: StreamBus, shm: Option<&ShmHeader>) {
    if let Some(p) = self.bus_profile(bus).cloned() {
      if let Err(e) = self.storage.save_profile(&p) {
        warn!("[OutputEq] could not save {bus:?} profile {}: {e}", p.name);
      }
    }
    self.notify(shm);
  }

  /// Persist settings and tell the other process.
  pub fn save_settings(&mut self, shm: Option<&ShmHeader>) {
    if let Err(e) = self.settings.save(&self.settings_path) {
      warn!("[OutputEq] could not save {}: {e}", self.settings_path.display());
    }
    self.notify(shm);
    self.re_resolve(shm);
    self.apply(shm);
  }

  /// Save the active headphone profile's edits (after a slider is released).
  pub fn save_headphone_profile(&mut self, shm: Option<&ShmHeader>) {
    if let Some(p) = self.headphone_profile().cloned() {
      self.save_headphone(&p);
    }
    self.notify(shm);
  }

  pub fn save_game_profile(&mut self, shm: Option<&ShmHeader>) {
    if let Some(p) = self.game_profile().cloned() {
      if let Err(e) = self.storage.save_profile(&p) {
        warn!("[OutputEq] could not save game profile {}: {e}", p.name);
      }
    }
    self.notify(shm);
  }

  /// Moves the profile file to `profiles/_deleted` (recoverable) and forgets every reference
  /// to it. Returns where the file went.
  pub fn delete_headphone_profile(&mut self, name: &str, shm: Option<&ShmHeader>) -> Result<PathBuf, String> {
    let path =
      self.storage.get_headphones_directory().join(format!("{}.json", ProfileStorage::sanitize_filename(name)));
    let moved = self.move_to_deleted(&path)?;
    self.headphone_profiles.retain(|p| p.name != name);
    self.settings.headphone_by_device.retain(|_, p| p != name);
    self.save_settings(shm);
    Ok(moved)
  }

  /// Same for a game profile; also drops bindings and the fallback that point to it.
  pub fn delete_game_profile(&mut self, name: &str, shm: Option<&ShmHeader>) -> Result<PathBuf, String> {
    let dir = self.storage.get_vad_directory(VadType::Game);
    let moved = self.move_to_deleted(&dir.join(format!("{}.json", ProfileStorage::sanitize_filename(name))))?;
    self.game_profiles.retain(|p| p.name != name);
    self.settings.bindings.retain(|_, p| p != name);
    if self.settings.fallback_game_profile.as_deref() == Some(name) {
      self.settings.fallback_game_profile = None;
    }
    self.save_settings(shm);
    Ok(moved)
  }

  /// Same for a chat / media profile; the channel falls back to flat if it was selected.
  pub fn delete_bus_profile(&mut self, bus: StreamBus, name: &str, shm: Option<&ShmHeader>) -> Result<PathBuf, String> {
    let vad = match bus {
      StreamBus::Chat => VadType::Chat,
      StreamBus::Media => VadType::Media,
      _ => return Err(format!("{bus:?} has no profiles")),
    };
    let dir = self.storage.get_vad_directory(vad);
    let moved = self.move_to_deleted(&dir.join(format!("{}.json", ProfileStorage::sanitize_filename(name))))?;
    let (list, selected) = match bus {
      StreamBus::Chat => (&mut self.chat_profiles, &mut self.settings.chat_profile),
      _ => (&mut self.media_profiles, &mut self.settings.media_profile),
    };
    list.retain(|p| p.name != name);
    if selected.as_deref() == Some(name) {
      *selected = None;
    }
    self.save_settings(shm);
    Ok(moved)
  }

  /// Add or replace a headphone profile (AutoEQ import) and use it on the current output.
  pub fn add_headphone_profile(&mut self, profile: ParametricProfile, shm: Option<&ShmHeader>) {
    self.save_headphone(&profile);
    self.headphone_profiles.retain(|p| p.name != profile.name);
    self.headphone_profiles.push(profile.clone());
    self.headphone_profiles.sort_by_key(|p| p.name.to_lowercase());
    self.select_headphone(Some(profile.name), shm);
  }

  pub fn select_headphone(&mut self, name: Option<String>, shm: Option<&ShmHeader>) {
    let Some(device) = self.output_device.clone() else {
      warn!("[OutputEq] no output device: headphone profile not assigned");
      return;
    };
    match name {
      Some(n) => self.settings.headphone_by_device.insert(device, n),
      None => self.settings.headphone_by_device.remove(&device),
    };
    self.save_settings(shm);
  }

  /// Bind an exe to a profile ("" = not a game). Re-resolves if it is the active game.
  pub fn bind_exe(&mut self, exe_name: &str, profile: Option<String>, shm: Option<&ShmHeader>) {
    let exe = exe_name.to_lowercase();
    match profile {
      Some(p) => self.settings.bindings.insert(exe.clone(), p),
      None => self.settings.bindings.remove(&exe),
    };
    for app in self.recent_apps.iter_mut().filter(|a| a.exe_name == exe) {
      app.matched = None;
    }
    self.save_settings(shm);
  }

  // --- events ------------------------------------------------------------------------------

  /// A window came to the foreground. `owner`: this process decides the active game.
  pub fn on_foreground(&mut self, event: &GameForegroundEvent, owner: bool, shm: Option<&ShmHeader>) {
    let exe = event.exe_name.to_lowercase();
    if exe == OWN_EXE {
      if owner {
        info!("[Game] foreground: this app, ignored; {}", self.active_game_text());
      }
      return;
    }
    let facts = self
      .facts_cache
      .entry(event.full_path.clone())
      .or_insert_with(|| launchers::gather_facts(&event.full_path))
      .clone();
    let matched = game_match::resolve(&self.settings, &self.game_profiles, &facts);
    let label = matched.as_ref().map(|m| m.label.clone()).unwrap_or_else(|| exe.clone());
    self.recent_apps.retain(|a| a.exe_name != exe);
    if self.recent_apps.len() >= RECENT_APPS {
      self.recent_apps.pop_back();
    }
    self.recent_apps.push_front(RecentApp { exe_name: exe.clone(), label, matched: matched.clone() });

    // Non-games (browser, Discord, this app) leave the active game in place.
    self.is_owner = owner;
    if !owner {
      return;
    }
    let how = game_match::explain(&self.settings, &facts, matched.as_ref());
    match matched {
      Some(m) => {
        info!("[Game] foreground: {exe}: {how}");
        let game = ActiveGame { profile: m.profile.clone().unwrap_or_default(), label: m.label.clone() };
        self.tracked = Some((event.pid, event.full_path.clone()));
        self.set_active_game(Some(game), shm);
      }
      None => info!("[Game] foreground: {exe}: {how}; {}", self.active_game_text()),
    }
  }

  /// Call often (UI frame / daemon loop). Follows the other process and the device.
  pub fn tick(&mut self, owner: bool, output_device: Option<String>, shm: Option<&ShmHeader>) {
    let mut dirty = false;
    self.is_owner = owner;
    if output_device != self.output_device {
      self.output_device = output_device;
      dirty = true;
    }
    if let Some(h) = shm {
      let version = h.out_settings_version.load(Ordering::Acquire);
      if self.seen_settings_version != Some(version) {
        // First tick only records the version; files were just loaded in `new`.
        if self.seen_settings_version.is_some() {
          self.reload();
          self.re_resolve(shm);
        }
        self.seen_settings_version = Some(version);
        dirty = true;
      }
      if !owner {
        let published = h.read_active_game();
        if published != self.active_game {
          self.active_game = published;
          dirty = true;
        }
      }
    }
    if owner && self.tracked.is_some() && self.last_alive_check.elapsed() >= ALIVE_CHECK_INTERVAL {
      self.last_alive_check = Instant::now();
      if self.tracked.as_ref().is_some_and(|(pid, _)| !is_process_running(*pid)) {
        info!("[Game] game exited");
        self.tracked = None;
        self.set_active_game(None, shm);
        return;
      }
    }
    if dirty {
      self.apply(shm);
    }
  }

  /// Forget a manual / detected game (e.g. "reset").
  /// Forget the active game; on a follower this asks nothing of the owner, so it only lasts
  /// until the owner publishes again.
  pub fn clear_active_game(&mut self, shm: Option<&ShmHeader>) {
    self.tracked = None;
    self.set_active_game(None, shm);
  }

  /// Re-read the profile folders (after a Sonar import).
  pub fn reload(&mut self) {
    self.settings = OutputEqSettings::load_or_default(&self.settings_path);
    self.headphone_profiles = self.storage.load_headphone_profiles();
    self.game_profiles = self.storage.load_profiles_for_vad(VadType::Game);
    self.chat_profiles = self.storage.load_profiles_for_vad(VadType::Chat);
    self.media_profiles = self.storage.load_profiles_for_vad(VadType::Media);
    self.facts_cache.clear();
  }

  // --- private helpers -------------------------------------------------------------------

  /// Owner only: classify the tracked game again after bindings / switches changed.
  fn re_resolve(&mut self, shm: Option<&ShmHeader>) {
    let Some((_, path)) = self.tracked.clone().filter(|_| self.is_owner) else {
      return;
    };
    let facts = self.facts_cache.entry(path.clone()).or_insert_with(|| launchers::gather_facts(&path)).clone();
    match game_match::resolve(&self.settings, &self.game_profiles, &facts) {
      Some(m) => {
        let game = ActiveGame { profile: m.profile.unwrap_or_default(), label: m.label };
        if self.active_game.as_ref() != Some(&game) {
          self.set_active_game(Some(game), shm);
        }
      }
      None => {
        self.tracked = None;
        self.set_active_game(None, shm);
      }
    }
  }

  fn set_active_game(&mut self, game: Option<ActiveGame>, shm: Option<&ShmHeader>) {
    if game != self.active_game {
      info!(
        "[Game] active game changed: {} -> {}",
        self.game_text(self.active_game.as_ref()),
        self.game_text(game.as_ref())
      );
    }
    if let Some(h) = shm {
      h.write_active_game(game.as_ref());
    }
    self.active_game = game;
    self.apply(shm);
  }

  /// For the log: what stays in effect when a non-game comes to the foreground.
  fn active_game_text(&self) -> String {
    format!("active game unchanged: {}", self.game_text(self.active_game.as_ref()))
  }

  /// "Label [profile P]" / "none [fallback profile F]", same choice as `game_profile_name`.
  fn game_text(&self, game: Option<&ActiveGame>) -> String {
    let fallback = self.settings.fallback_game_profile.as_deref();
    match game {
      Some(g) if !g.profile.is_empty() => format!("{} [profile {}]", g.label, g.profile),
      Some(g) => match fallback {
        Some(f) => format!("{} [no own profile, fallback {f}]", g.label),
        None => format!("{} [no profile]", g.label),
      },
      None => match fallback {
        Some(f) => format!("none [fallback {f}]"),
        None => "none [no game EQ]".to_string(),
      },
    }
  }

  fn notify(&mut self, shm: Option<&ShmHeader>) {
    if let Some(h) = shm {
      let v = h.out_settings_version.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
      self.seen_settings_version = Some(v);
    }
  }

  fn save_headphone(&self, profile: &ParametricProfile) {
    let dir = self.storage.get_headphones_directory();
    let path = dir.join(format!("{}.json", ProfileStorage::sanitize_filename(&profile.name)));
    let result = serde_json::to_string_pretty(profile)
      .map_err(std::io::Error::other)
      .and_then(|json| settings::write_atomic(&path, &json));
    if let Err(e) = result {
      warn!("[OutputEq] could not save {}: {e}", path.display());
    }
  }

  fn move_to_deleted(&self, path: &std::path::Path) -> Result<PathBuf, String> {
    let file_name = path.file_name().ok_or("bad profile path")?;
    let dir = self.storage.base_dir().join(DELETED_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let mut target = dir.join(file_name);
    if target.exists() {
      // Keep older deleted copies: "<name>.json" -> "<name>.<unix time>.json".
      let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
      target = dir.join(format!("{}.{stamp}.json", path.file_stem().unwrap_or_default().to_string_lossy()));
    }
    std::fs::rename(path, &target).map_err(|e| format!("could not move {}: {e}", path.display()))?;
    info!("[OutputEq] profile moved to {}", target.display());
    Ok(target)
  }

  /// Curves + the volume change of headphone + game.
  fn recompute(&mut self) {
    let s = &self.settings;
    let hp = if s.enabled && s.headphone_enabled { self.headphone_profile() } else { None };
    let game = if s.enabled && s.game_enabled { self.game_profile() } else { None };
    let (result, preamp, peak) = self.volume_change(hp, game);
    (self.result, self.preamp_db, self.peak_after_db) = (result, preamp, peak);
  }

  /// Curves of `hp` + `layer`, their volume change (automatic preamp, safe or
  /// loudness-matched, plus both profiles' own gains) and the peak after it.
  fn volume_change(
    &self,
    hp: Option<&ParametricProfile>,
    layer: Option<&ParametricProfile>,
  ) -> (CombinedEngineResult, f32, f32) {
    let bands = |p: Option<&ParametricProfile>| p.map(|p| p.bands.clone()).unwrap_or_default();
    let (hp_bands, layer_bands) = (bands(hp), bands(layer));
    let user_gain = hp.map_or(0.0, |p| p.gain_db) + layer.map_or(0.0, |p| p.gain_db);
    let has_eq = hp_bands.iter().chain(&layer_bands).any(|b| b.enabled);
    let result = compute_combined_engine(&hp_bands, &layer_bands, 48_000.0);
    let max_boost = result.max_boost_db;
    let auto = match (has_eq, self.settings.loudness_match) {
      (false, _) => 0.0,
      (true, false) => result.auto_preamp_db,
      (true, true) => (-average_gain(&result)).min(MATCH_MAX_PEAK_DB - max_boost),
    };
    let preamp = (auto + user_gain).clamp(PREAMP_RANGE_DB.0, PREAMP_RANGE_DB.1);
    (result, preamp, max_boost + preamp)
  }

  /// (headphone, channel profile) a chat/media stream should end up with.
  fn bus_layers(&self, bus: StreamBus) -> (Option<&ParametricProfile>, Option<&ParametricProfile>) {
    let s = &self.settings;
    let hp = if s.enabled && s.headphone_enabled { self.headphone_profile() } else { None };
    let own = if s.enabled && self.bus_enabled(bus) { self.bus_profile(bus) } else { None };
    (hp, own)
  }

  /// Stream EQ of a chat/media stream: its own profile, then the game stages undone; the
  /// preamp turns the output's volume change into the channel's own.
  fn stream_eq(&self, bus: StreamBus, game_applied: &[BiquadBand], out_preamp_db: f32) -> (Vec<BiquadBand>, f32) {
    let (hp, own) = self.bus_layers(bus);
    let mut stages: Vec<BiquadBand> =
      expand_slopes(&own.map(|p| p.bands.clone()).unwrap_or_default()).into_iter().filter(|b| b.enabled).collect();
    stages.extend(invert_bands(game_applied));
    if stages.len() > MAX_STREAM_EQ_BANDS {
      warn!("[OutputEq] {bus:?} stream EQ: {} stages, only the first {MAX_STREAM_EQ_BANDS} are applied", stages.len());
      stages.truncate(MAX_STREAM_EQ_BANDS);
    }
    let own_preamp = self.volume_change(hp, own).1;
    (stages, own_preamp - out_preamp_db)
  }
}

/// Mean gain (dB) of the combined curve over the loudness band.
fn average_gain(result: &CombinedEngineResult) -> f32 {
  let c = &result.combined_curve;
  let (sum, n) = c
    .frequencies
    .iter()
    .zip(&c.gains_db)
    .filter(|(f, _)| (LOUDNESS_BAND_HZ.0..=LOUDNESS_BAND_HZ.1).contains(*f))
    .fold((0.0f32, 0usize), |(s, n), (_, g)| (s + g, n + 1));
  if n == 0 {
    0.0
  } else {
    sum / n as f32
  }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
  use super::*;
  use gamersequalizer_common::profile::FilterType;

  fn header() -> Box<ShmHeader> {
    Box::new(ShmHeader::default())
  }

  fn engine(tag: &str) -> (OutputEq, PathBuf) {
    let dir = std::env::temp_dir().join(format!("ge_output_eq_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let storage = ProfileStorage::new(&dir);
    let mut game = ParametricProfile::new("The Division 2", VadType::Game);
    game.game_ids = vec!["1_2221490".into()];
    game.bands = vec![BiquadBand { gain_db: 4.0, frequency: 3000.0, ..BiquadBand::default() }];
    storage.save_profile(&game).unwrap();
    let mut eq = OutputEq::new(storage);
    eq.output_device = Some("dev-1".into());
    let mut hp = ParametricProfile::new("Rode NTH-100", VadType::Media);
    hp.bands = vec![BiquadBand {
      filter_type: FilterType::LowShelf,
      gain_db: 3.0,
      frequency: 105.0,
      q: 0.7,
      enabled: true,
      slope_db: 12,
    }];
    eq.add_headphone_profile(hp, None);
    (eq, dir)
  }

  #[test]
  fn layers_master_switch_and_ab_toggle() {
    let (mut eq, dir) = engine("layers");
    let h = header();
    eq.apply(Some(&h));
    assert_eq!(h.read_out_eq().0.len(), 1, "headphone only, no game yet");
    assert!(h.read_out_eq().1 < 0.0, "boost needs a negative preamp");

    let event = GameForegroundEvent {
      pid: std::process::id(),
      exe_name: "division2.exe".into(),
      full_path: r"C:\x\division2.exe".into(),
    };
    eq.on_foreground(&event, true, Some(&h));
    assert_eq!(h.read_active_game().unwrap().profile, "The Division 2");
    assert_eq!(h.read_out_eq().0.len(), 2, "headphone + game");

    eq.settings.headphone_enabled = false;
    eq.save_settings(Some(&h));
    assert_eq!(h.read_out_eq().0.len(), 1, "A/B: headphone layer off");

    eq.settings.enabled = false;
    eq.save_settings(Some(&h));
    assert_eq!(h.read_out_eq(), (vec![], 0.0));
    assert!(h.master_bypass.load(Ordering::Relaxed));
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn chat_stream_eq_is_own_profile_plus_inverted_game() {
    let (mut eq, dir) = engine("stream");
    let h = header();
    let event = GameForegroundEvent {
      pid: std::process::id(),
      exe_name: "division2.exe".into(),
      full_path: r"C:\x\division2.exe".into(),
    };
    eq.on_foreground(&event, true, Some(&h));
    let (out_bands, out_preamp) = h.read_out_eq();
    assert_eq!(out_bands.len(), 2, "headphone + game");

    // No chat profile: the chat stream only undoes the game band (+4 dB at 3 kHz).
    let (chat, chat_preamp) = h.stream_eq[0].read();
    assert_eq!(chat.len(), 1);
    assert_eq!((chat[0].frequency, chat[0].gain_db), (3000.0, -4.0));
    let hp_only = eq.volume_change(eq.headphone_profile(), None).1;
    assert!((chat_preamp - (hp_only - out_preamp)).abs() < 1e-4, "volume as with headphone only");

    // With a chat profile: its band first, then the inverse.
    let mut voice = ParametricProfile::new("Voice", VadType::Chat);
    voice.bands = vec![BiquadBand { gain_db: 2.0, frequency: 2000.0, ..BiquadBand::default() }];
    eq.chat_profiles.push(voice);
    eq.select_bus_profile(StreamBus::Chat, Some("Voice".into()), Some(&h));
    let gains: Vec<f32> = h.stream_eq[0].read().0.iter().map(|b| b.gain_db).collect();
    assert_eq!(gains, vec![2.0, -4.0]);
    assert_eq!(h.stream_eq[1].read().0.len(), 1, "media: inverse only");

    // Game layer off: nothing to undo. Master off: pass-through.
    eq.settings.game_enabled = false;
    eq.save_settings(Some(&h));
    assert!(h.stream_eq[1].read().0.is_empty());
    eq.settings.enabled = false;
    eq.save_settings(Some(&h));
    assert_eq!(h.stream_eq[0].read(), (vec![], 0.0));
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn loudness_matching_profile_gain_and_safe_mode() {
    let (mut eq, dir) = engine("gain");
    let h = header();
    // Headphone profile: +3 dB low shelf only. Safe mode compensates the whole peak.
    eq.settings.loudness_match = false;
    eq.apply(Some(&h));
    let safe = h.read_out_eq().1;
    assert!((safe + 3.5).abs() < 0.3, "safe preamp ~ -(3 + 0.5), got {safe}");
    // Matching: the boost is below 200 Hz, so the mid band barely needs compensation.
    eq.settings.loudness_match = true;
    eq.apply(Some(&h));
    let matched = h.read_out_eq().1;
    assert!(matched > safe + 2.0, "matched {matched} should be louder than safe {safe}");
    assert!(eq.peak_after_db <= MATCH_MAX_PEAK_DB + 0.01);
    // The profile's own gain adds on top.
    eq.headphone_profile_mut().unwrap().gain_db = -2.0;
    eq.apply(Some(&h));
    assert!((h.read_out_eq().1 - (matched - 2.0)).abs() < 0.01);
    // A flat profile with gain only still changes the volume.
    eq.headphone_profile_mut().unwrap().bands.clear();
    eq.apply(Some(&h));
    assert_eq!(h.read_out_eq(), (vec![], -2.0));
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn deleting_moves_the_file_and_drops_references() {
    let (mut eq, dir) = engine("delete");
    eq.settings.fallback_game_profile = Some("The Division 2".into());
    eq.settings.bindings.insert("x.exe".into(), "The Division 2".into());
    let moved = eq.delete_headphone_profile("Rode NTH-100", None).unwrap();
    assert!(moved.exists() && moved.starts_with(dir.join(DELETED_DIR)));
    assert_eq!(eq.headphone_profile_name(), None);
    assert!(eq.headphone_profiles.is_empty());

    eq.delete_game_profile("The Division 2", None).unwrap();
    assert!(eq.game_profiles.is_empty() && eq.settings.bindings.is_empty());
    assert_eq!(eq.settings.fallback_game_profile, None);
    // Nothing left to delete: an error, no panic.
    assert!(eq.delete_game_profile("The Division 2", None).is_err());
    // Reloading from disk confirms the files are gone from the profile folders.
    eq.reload();
    assert!(eq.game_profiles.is_empty() && eq.headphone_profiles.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn follower_picks_up_owner_game_and_saved_settings() {
    let (mut owner, dir) = engine("follow");
    let mut follower = OutputEq::new(ProfileStorage::new(&dir));
    let h = header();
    owner.tick(true, Some("dev-1".into()), Some(&h));
    follower.tick(false, Some("dev-1".into()), Some(&h));
    assert_eq!(follower.headphone_profile_name(), Some("Rode NTH-100"));

    let event = GameForegroundEvent {
      pid: std::process::id(),
      exe_name: "division2.exe".into(),
      full_path: r"C:\x\division2.exe".into(),
    };
    follower.on_foreground(&event, false, Some(&h));
    assert_eq!(h.read_active_game(), None, "only the owner decides");
    owner.on_foreground(&event, true, Some(&h));
    follower.tick(false, Some("dev-1".into()), Some(&h));
    assert_eq!(follower.game_profile_name(), Some("The Division 2"));

    owner.settings.game_enabled = false;
    owner.save_settings(Some(&h));
    follower.tick(false, Some("dev-1".into()), Some(&h));
    assert!(!follower.settings.game_enabled, "follower reloaded the saved settings");

    owner.output_device = None;
    owner.tick(true, Some("dev-2".into()), Some(&h));
    assert_eq!(owner.headphone_profile_name(), None, "headphone profile is per output device");
    let _ = std::fs::remove_dir_all(&dir);
  }
}
