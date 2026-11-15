//! Built-in game profiles by genre, for games without a Sonar preset (or without Sonar).
//! Written once into `profiles/game` as ordinary profiles: editable, deletable, assignable.
//! Names are English translation keys; the UI shows them translated.

use super::genre::Genre;
use super::settings::{OutputEqSettings, SETTINGS_FILE};
use crate::sonar_importer::ProfileStorage;
use gamersequalizer_common::profile::{BiquadBand, FilterType, ParametricProfile, VadType};
use log::{info, warn};

/// Bump when presets are added; existing files (user edits) are never overwritten and
/// deleted ones only come back when this changes.
pub const BUILTIN_GAME_PRESETS_VERSION: u32 = 2;

pub fn builtin_game_presets() -> Vec<ParametricProfile> {
  vec![
    // Footsteps, reloads and directional cues live at 2-5 kHz; explosions are tamed.
    preset(
      Genre::Shooter.preset_name(),
      vec![
        band(FilterType::LowShelf, 120.0, -3.0, 0.707),
        pk(250.0, -1.5, 1.0),
        pk(2500.0, 4.0, 1.2),
        pk(4500.0, 3.0, 1.5),
        band(FilterType::HighShelf, 10000.0, -1.0, 0.707),
      ],
    ),
    // Everything for information, nothing for atmosphere.
    preset(
      Genre::Competitive.preset_name(),
      vec![
        band(FilterType::LowShelf, 150.0, -6.0, 0.707),
        pk(1000.0, -1.0, 1.0),
        pk(3000.0, 5.0, 1.0),
        pk(6000.0, 3.0, 2.0),
        band(FilterType::HighShelf, 12000.0, -2.0, 0.707),
      ],
    ),
    // Mild V: more weight and air, voices untouched.
    preset(
      Genre::OpenWorld.preset_name(),
      vec![
        band(FilterType::LowShelf, 80.0, 3.0, 0.707),
        pk(3000.0, 1.0, 1.0),
        band(FilterType::HighShelf, 10000.0, 2.0, 0.707),
      ],
    ),
    // Voices forward, less boom and hiss.
    preset(Genre::Story.preset_name(), vec![pk(200.0, -2.0, 1.0), pk(2000.0, 3.0, 1.0), pk(7000.0, -1.0, 2.0)]),
    // Top-down games: ability and UI cues sit around 4 kHz; less mud, bass kept.
    preset(
      Genre::Moba.preset_name(),
      vec![
        band(FilterType::LowShelf, 100.0, -2.0, 0.707),
        pk(250.0, -2.0, 1.0),
        pk(1000.0, 1.5, 1.0),
        pk(4000.0, 5.0, 1.2),
        band(FilterType::HighShelf, 10000.0, -1.0, 0.707),
      ],
    ),
  ]
}

/// Writes the built-ins missing from `profiles/game`, once per preset version.
pub fn seed_builtin_game_presets(storage: &ProfileStorage) {
  let settings_path = storage.base_dir().join(SETTINGS_FILE);
  let mut settings = OutputEqSettings::load_or_default(&settings_path);
  if settings.builtin_game_presets >= BUILTIN_GAME_PRESETS_VERSION {
    return;
  }
  let dir = storage.get_vad_directory(VadType::Game);
  for preset in builtin_game_presets() {
    if dir.join(format!("{}.json", ProfileStorage::sanitize_filename(&preset.name))).exists() {
      continue; // the user's edited copy wins
    }
    match storage.save_profile(&preset) {
      Ok(path) => info!("[OutputEq] built-in game profile written: {}", path.display()),
      Err(e) => warn!("[OutputEq] could not write built-in game profile {}: {e}", preset.name),
    }
  }
  settings.builtin_game_presets = BUILTIN_GAME_PRESETS_VERSION;
  if let Err(e) = settings.save(&settings_path) {
    warn!("[OutputEq] could not save {}: {e}", settings_path.display());
  }
}

fn preset(name: &str, bands: Vec<BiquadBand>) -> ParametricProfile {
  let mut p = ParametricProfile::new(name, VadType::Game);
  p.bands = bands;
  p.is_custom = false;
  p
}

fn pk(freq: f32, gain_db: f32, q: f32) -> BiquadBand {
  band(FilterType::Peaking, freq, gain_db, q)
}

fn band(filter_type: FilterType, frequency: f32, gain_db: f32, q: f32) -> BiquadBand {
  BiquadBand { enabled: true, filter_type, frequency, gain_db, q, slope_db: 12 }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
  use super::super::game_match::{resolve, GameFacts};
  use super::*;
  use gamersequalizer_common::biquad::MAX_LAYER_EQ_BANDS;

  #[test]
  fn presets_fit_the_game_layer_and_parameter_ranges() {
    for p in builtin_game_presets() {
      assert!(p.bands.len() <= MAX_LAYER_EQ_BANDS, "{}", p.name);
      for b in &p.bands {
        let mut clamped = b.clone();
        clamped.clamp_parameters();
        assert_eq!(&clamped, b, "{}", p.name);
      }
    }
  }

  #[test]
  fn seeding_runs_once_and_keeps_user_edits() {
    let dir = std::env::temp_dir().join(format!("ge_game_presets_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let storage = ProfileStorage::new(&dir);
    let mut edited = builtin_game_presets().remove(0);
    edited.gain_db = -4.0;
    storage.save_profile(&edited).unwrap();

    seed_builtin_game_presets(&storage);
    let loaded = storage.load_profiles_for_vad(VadType::Game);
    assert_eq!(loaded.len(), builtin_game_presets().len());
    assert_eq!(loaded.iter().find(|p| p.name == edited.name).unwrap().gain_db, -4.0, "edit kept");

    // Deleted by the user: not written again by the same version.
    let game_dir = storage.get_vad_directory(VadType::Game);
    for entry in std::fs::read_dir(&game_dir).unwrap() {
      std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    seed_builtin_game_presets(&storage);
    assert!(storage.load_profiles_for_vad(VadType::Game).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn genre_names_never_auto_match_a_game() {
    let profiles = builtin_game_presets();
    for (exe, names) in [
      ("division2.exe", vec!["Tom Clancy's The Division 2"]),
      ("shooter.exe", vec!["Shooter"]),
      ("cs2.exe", vec!["Counter-Strike 2"]),
      ("openworld.exe", vec!["Open World"]),
    ] {
      let facts = GameFacts {
        exe_name: exe.into(),
        launcher_ids: vec![],
        names: names.into_iter().map(String::from).collect(),
        from_launcher: false,
        genre: None,
        not_a_game: None,
      };
      assert_eq!(resolve(&OutputEqSettings::default(), &profiles, &facts), None, "{exe}");
    }
  }
}
