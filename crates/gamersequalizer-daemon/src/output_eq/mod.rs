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

pub mod auto_preamp;
pub mod autoeq;
pub mod game_match;
pub mod game_presets;
pub mod genre;
pub mod launchers;
pub mod settings;
pub mod share_text;

use crate::sonar_importer::ProfileStorage;
use crate::win_audio::game_detector::{GameForegroundEvent, is_process_running};
use auto_preamp::{CombinedEngineResult, compute_combined_engine};
use game_match::{GameFacts, GameMatch};
use gamersequalizer_common::biquad::{MAX_OUT_EQ_BANDS, MAX_STREAM_EQ_BANDS};
use gamersequalizer_common::profile::{BiquadBand, ParametricProfile, VadType, expand_slopes, invert_bands};
use gamersequalizer_common::shm::{ActiveGame, ShmHeader};
use gamersequalizer_common::streams::StreamBus;
use log::{info, warn};
use settings::OutputEqSettings;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

mod detection;
mod profiles;
mod publish;

pub use detection::*;
pub use profiles::*;

const RECENT_APPS: usize = 12;

/// Loudness matching lets EQ peaks go at most this far above 0 dBFS (the APO's soft limiter
/// handles them); a stronger boost is still partly compensated.
const MATCH_MAX_PEAK_DB: f32 = 3.0;

/// Which layer of the output EQ a profile / edit belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputLayer {
  Headphone,
  Game,
  Chat,
  Media,
}

impl OutputLayer {
  /// The stream channel of a chat / media layer.
  pub fn bus(self) -> Option<StreamBus> {
    match self {
      OutputLayer::Chat => Some(StreamBus::Chat),
      OutputLayer::Media => Some(StreamBus::Media),
      OutputLayer::Headphone | OutputLayer::Game => None,
    }
  }

  /// Profile storage category for game / chat / media (None = headphones folder).
  pub fn vad(self) -> Option<VadType> {
    match self {
      OutputLayer::Headphone => None,
      OutputLayer::Game => Some(VadType::Game),
      OutputLayer::Chat => Some(VadType::Chat),
      OutputLayer::Media => Some(VadType::Media),
    }
  }
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
  genres: genre::GenreLookup,
  seen_settings_version: Option<u32>,
}

impl OutputEq {
  pub fn new(storage: ProfileStorage) -> Self {
    let settings_path = storage.base_dir().join(settings::SETTINGS_FILE);
    let settings = OutputEqSettings::load_or_default(&settings_path);
    let genres = genre::GenreLookup::new(storage.base_dir());
    let mut engine = Self {
      headphone_profiles: storage.load_layer_profiles(OutputLayer::Headphone),
      game_profiles: storage.load_layer_profiles(OutputLayer::Game),
      chat_profiles: storage.load_layer_profiles(OutputLayer::Chat),
      media_profiles: storage.load_layer_profiles(OutputLayer::Media),
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
      genres,
      seen_settings_version: None,
    };
    engine.recompute();
    engine
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

  /// Re-read the profile folders (after a Sonar import).
  pub fn reload(&mut self) {
    self.settings = OutputEqSettings::load_or_default(&self.settings_path);
    for layer in [OutputLayer::Headphone, OutputLayer::Game, OutputLayer::Chat, OutputLayer::Media] {
      *self.layer_profiles_mut(layer) = self.storage.load_layer_profiles(layer);
    }
    self.facts_cache.clear();
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::sonar_importer::DELETED_DIR;
  use gamersequalizer_common::profile::FilterType;

  fn header() -> Box<ShmHeader> {
    Box::new(ShmHeader::default())
  }

  /// A lazy band reader's result, comparable with `assert_eq!`.
  fn collected((bands, preamp): (impl Iterator<Item = BiquadBand>, f32)) -> (Vec<BiquadBand>, f32) {
    (bands.collect(), preamp)
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
    assert_eq!(collected(h.read_out_eq()).0.len(), 1, "headphone only, no game yet");
    assert!(collected(h.read_out_eq()).1 < 0.0, "boost needs a negative preamp");

    let event = GameForegroundEvent {
      pid: std::process::id(),
      exe_name: "division2.exe".into(),
      full_path: r"C:\x\division2.exe".into(),
    };
    eq.on_foreground(&event, true, Some(&h));
    assert_eq!(h.read_active_game().unwrap().profile, "The Division 2");
    assert_eq!(collected(h.read_out_eq()).0.len(), 2, "headphone + game");

    eq.settings.headphone_enabled = false;
    eq.save_settings(Some(&h));
    assert_eq!(collected(h.read_out_eq()).0.len(), 1, "A/B: headphone layer off");

    eq.settings.enabled = false;
    eq.save_settings(Some(&h));
    assert_eq!(collected(h.read_out_eq()), (vec![], 0.0));
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
    let (out_bands, out_preamp) = collected(h.read_out_eq());
    assert_eq!(out_bands.len(), 2, "headphone + game");

    // No chat profile: the chat stream only undoes the game band (+4 dB at 3 kHz).
    let (chat, chat_preamp) = collected(h.stream_eq[0].read());
    assert_eq!(chat.len(), 1);
    assert_eq!((chat[0].frequency, chat[0].gain_db), (3000.0, -4.0));
    let hp_only = eq.volume_change(eq.headphone_profile(), None).1;
    assert!((chat_preamp - (hp_only - out_preamp)).abs() < 1e-4, "volume as with headphone only");

    // With a chat profile: its band first, then the inverse.
    let mut voice = ParametricProfile::new("Voice", VadType::Chat);
    voice.bands = vec![BiquadBand { gain_db: 2.0, frequency: 2000.0, ..BiquadBand::default() }];
    eq.chat_profiles.push(voice);
    eq.select_bus_profile(StreamBus::Chat, Some("Voice".into()), Some(&h));
    let gains: Vec<f32> = collected(h.stream_eq[0].read()).0.iter().map(|b| b.gain_db).collect();
    assert_eq!(gains, vec![2.0, -4.0]);
    assert_eq!(collected(h.stream_eq[1].read()).0.len(), 1, "media: inverse only");

    // Game layer off: nothing to undo. Master off: pass-through.
    eq.settings.game_enabled = false;
    eq.save_settings(Some(&h));
    assert!(collected(h.stream_eq[1].read()).0.is_empty());
    eq.settings.enabled = false;
    eq.save_settings(Some(&h));
    assert_eq!(collected(h.stream_eq[0].read()), (vec![], 0.0));
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn loudness_matching_profile_gain_and_safe_mode() {
    let (mut eq, dir) = engine("gain");
    let h = header();
    // Headphone profile: +3 dB low shelf only. Safe mode compensates the whole peak.
    eq.settings.loudness_match = false;
    eq.apply(Some(&h));
    let safe = collected(h.read_out_eq()).1;
    assert!((safe + 3.5).abs() < 0.3, "safe preamp ~ -(3 + 0.5), got {safe}");
    // Matching: the boost is below 200 Hz, so the mid band barely needs compensation.
    eq.settings.loudness_match = true;
    eq.apply(Some(&h));
    let matched = collected(h.read_out_eq()).1;
    assert!(matched > safe + 2.0, "matched {matched} should be louder than safe {safe}");
    assert!(eq.peak_after_db <= MATCH_MAX_PEAK_DB + 0.01);
    // The profile's own gain adds on top.
    eq.headphone_profile_mut().unwrap().gain_db = -2.0;
    eq.apply(Some(&h));
    assert!((collected(h.read_out_eq()).1 - (matched - 2.0)).abs() < 0.01);
    // A flat profile with gain only still changes the volume.
    eq.headphone_profile_mut().unwrap().bands.clear();
    eq.apply(Some(&h));
    assert_eq!(collected(h.read_out_eq()), (vec![], -2.0));
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn deleting_moves_the_file_and_drops_references() {
    let (mut eq, dir) = engine("delete");
    eq.settings.fallback_game_profile = Some("The Division 2".into());
    eq.settings.bindings.insert("x.exe".into(), "The Division 2".into());
    let moved = eq.delete_layer_profile(OutputLayer::Headphone, "Rode NTH-100", None).unwrap();
    assert!(moved.exists() && moved.starts_with(dir.join(DELETED_DIR)));
    assert_eq!(eq.headphone_profile_name(), None);
    assert!(eq.headphone_profiles.is_empty());

    eq.delete_layer_profile(OutputLayer::Game, "The Division 2", None).unwrap();
    assert!(eq.game_profiles.is_empty() && eq.settings.bindings.is_empty());
    assert_eq!(eq.settings.fallback_game_profile, None);
    // Nothing left to delete: an error, no panic.
    assert!(eq.delete_layer_profile(OutputLayer::Game, "The Division 2", None).is_err());
    // Reloading from disk confirms the files are gone from the profile folders.
    // A chat profile in use: the channel goes back to flat.
    eq.add_layer_profile(OutputLayer::Chat, ParametricProfile::new("Voice", VadType::Chat), None).unwrap();
    assert_eq!(eq.bus_profile_name(StreamBus::Chat), Some("Voice"));
    eq.delete_layer_profile(OutputLayer::Chat, "Voice", None).unwrap();
    assert_eq!(eq.bus_profile_name(StreamBus::Chat), None);
    eq.reload();
    assert!(eq.game_profiles.is_empty() && eq.headphone_profiles.is_empty() && eq.chat_profiles.is_empty());
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

  #[test]
  fn layer_add_save_and_delete_works_across_all_output_layers() {
    let (mut eq, dir) = engine("layers_crud");
    let h = header();
    assert!(eq.add_layer_profile(OutputLayer::Chat, ParametricProfile::new("   ", VadType::Chat), None).is_err());

    let mut chat = ParametricProfile::new("Discord", VadType::Chat);
    chat.bands.push(BiquadBand { frequency: 2000.0, gain_db: 2.5, ..BiquadBand::default() });
    eq.add_layer_profile(OutputLayer::Chat, chat, Some(&h)).unwrap();
    assert_eq!(eq.layer_profile_name(OutputLayer::Chat), Some("Discord"));
    assert!(eq.layer_profile_exists(OutputLayer::Chat, "discord"));

    eq.layer_profile_mut(OutputLayer::Chat).unwrap().gain_db = -1.5;
    eq.save_layer_profile(OutputLayer::Chat, Some(&h));

    let media = ParametricProfile::new("Music", VadType::Media);
    eq.add_layer_profile(OutputLayer::Media, media, Some(&h)).unwrap();
    assert_eq!(eq.layer_profile_name(OutputLayer::Media), Some("Music"));

    // Game without active game selects as fallback.
    let fallback = ParametricProfile::new("General", VadType::Game);
    eq.add_layer_profile(OutputLayer::Game, fallback, Some(&h)).unwrap();
    assert_eq!(eq.settings.fallback_game_profile.as_deref(), Some("General"));

    let reloaded = OutputEq::new(ProfileStorage::new(&dir));
    assert_eq!(reloaded.layer_profile(OutputLayer::Chat).unwrap().gain_db, -1.5);
    assert!(reloaded.layer_profile_exists(OutputLayer::Media, "Music"));

    eq.delete_layer_profile(OutputLayer::Chat, "Discord", Some(&h)).unwrap();
    assert_eq!(eq.layer_profile_name(OutputLayer::Chat), None);
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn profile_names_collide_like_windows_file_names() {
    assert!(same_profile_slot("Řeč", "řeč"));
    assert!(same_profile_slot("Discord ", "discord"));
    assert!(same_profile_slot("a:b", "A_B"), "same file after sanitizing");
    assert!(!same_profile_slot("Řeč", "Rec"));
  }

  #[test]
  fn replacing_a_game_profile_keeps_its_ids_spelling_and_bindings() {
    let (mut eq, dir) = engine("replace");
    let h = header();
    let event = GameForegroundEvent {
      pid: std::process::id(),
      exe_name: "division2.exe".into(),
      full_path: r"C:\x\division2.exe".into(),
    };
    eq.on_foreground(&event, true, Some(&h));
    let bindings = eq.settings.bindings.clone();

    let mut edited = ParametricProfile::new("the division 2", VadType::Game);
    edited.bands = vec![BiquadBand { gain_db: -3.0, frequency: 500.0, ..BiquadBand::default() }];
    eq.add_layer_profile(OutputLayer::Game, edited, Some(&h)).unwrap();
    assert_eq!(eq.game_profiles.len(), 1, "replaced, not added");
    let p = eq.game_profiles.first().unwrap();
    assert_eq!(p.name, "The Division 2", "the existing spelling stays");
    assert_eq!(p.game_ids, vec!["1_2221490".to_string()]);
    assert_eq!(p.bands.first().map(|b| b.gain_db), Some(-3.0));

    // A new profile while the game runs: saved, but the game stays on its own profile.
    eq.add_layer_profile(OutputLayer::Game, ParametricProfile::new("Flat", VadType::Game), Some(&h)).unwrap();
    assert_eq!(eq.game_profile_name(), Some("The Division 2"));
    assert_eq!(eq.settings.bindings, bindings);
    assert!(eq.layer_profile_exists(OutputLayer::Game, "flat"));

    let reloaded = OutputEq::new(ProfileStorage::new(&dir));
    assert_eq!(reloaded.game_profiles.iter().find(|p| p.name == "The Division 2").unwrap().game_ids.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
  }
}
