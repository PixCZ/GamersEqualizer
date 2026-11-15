//! Microphone EQ presets: the user's own (saved by name, listed first), built-in voice presets,
//! and the ones imported from Sonar. Sharing as text: `output_eq::share_text`.
//!
//! Cuts are one band with a steepness (12/24/48 dB/oct, see `BiquadBand::slope_db`): a steep
//! low cut drops handling noise, desk bumps and rumble fast, a high cut drops hiss, while the
//! voice band (~100 Hz .. 8 kHz) stays untouched.

use std::io;
use std::path::PathBuf;

use crate::output_eq::same_profile_slot;
use crate::sonar_importer::ProfileStorage;
use gamersequalizer_common::profile::{BiquadBand, FilterType, ParametricProfile, VadType, collapse_slopes};

use crate::i18n::tk;

/// Low cuts in the presets: very steep, handling noise sits just below the voice.
const LOW_CUT_SLOPE_DB: u8 = 48;
const HIGH_CUT_SLOPE_DB: u8 = 24;
/// The user's own presets, next to the Sonar imports (`profiles/mic`), never mixed with them.
const USER_DIR: &str = "mic_user";

/// The default when no saved mic EQ exists: cut what is not voice, change nothing else.
pub const DEFAULT_PRESET: &str = tk("Clean voice (cut rumble & hiss)");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetSource {
  User,
  Builtin,
  Sonar,
}

#[derive(Debug, Clone)]
pub struct MicPreset {
  pub source: PresetSource,
  pub profile: ParametricProfile,
}

/// Can a preset be saved under this name?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameCheck {
  Ok,
  Empty,
  /// One of the user's presets has this name: saving replaces it.
  Overwrite,
  /// A built-in or Sonar preset has it (they can't be replaced; names must stay unique).
  Taken(PresetSource),
}

/// What the preset picker lists: the user's first, then built-ins, then Sonar imports.
pub fn all_mic_presets(storage: &ProfileStorage) -> Vec<MicPreset> {
  let tag = |source| move |profile| MicPreset { source, profile };
  let mut list: Vec<MicPreset> = storage
    .load_profiles_from_dir(&user_dir(storage))
    .into_iter()
    .map(|mut p| {
      // Saved before cuts had a steepness: two stacked rows become one.
      p.bands = collapse_slopes(&p.bands);
      p
    })
    .map(tag(PresetSource::User))
    .collect();
  list.extend(builtin_mic_presets().into_iter().map(tag(PresetSource::Builtin)));
  list.extend(storage.load_profiles_for_vad(VadType::Mic).into_iter().map(tag(PresetSource::Sonar)));
  list
}

pub fn find<'a>(list: &'a [MicPreset], name: &str) -> Option<&'a MicPreset> {
  list.iter().find(|p| p.profile.name == name)
}

/// The current EQ differs from the preset it came from, or that preset no longer exists
/// (then it is saved nowhere but the active-EQ file).
pub fn is_modified(list: &[MicPreset], current: &ParametricProfile) -> bool {
  find(list, &current.name).is_none_or(|p| p.profile.bands != current.bands)
}

pub fn check_new_name(list: &[MicPreset], name: &str) -> NameCheck {
  let name = name.trim();
  if name.is_empty() {
    return NameCheck::Empty;
  }
  let hit = find(list, name).or_else(|| list.iter().find(|p| same_profile_slot(&p.profile.name, name)));
  match hit {
    None => NameCheck::Ok,
    Some(p) if p.source == PresetSource::User => NameCheck::Overwrite,
    Some(p) => NameCheck::Taken(p.source),
  }
}

/// Writes `profile` as one of the user's presets (replacing one with the same name).
pub fn save_user_preset(storage: &ProfileStorage, profile: &ParametricProfile) -> io::Result<()> {
  let dir = user_dir(storage);
  std::fs::create_dir_all(&dir)?;
  let mut p = profile.clone();
  p.vad_type = VadType::Mic;
  p.is_custom = true;
  let json = serde_json::to_string_pretty(&p).map_err(io::Error::other)?;
  crate::output_eq::settings::write_atomic(&user_file(storage, &p.name), &json)
}

pub fn delete_user_preset(storage: &ProfileStorage, name: &str) -> io::Result<()> {
  std::fs::remove_file(user_file(storage, name))
}

pub fn builtin_mic_presets() -> Vec<ParametricProfile> {
  vec![
    preset(DEFAULT_PRESET, [steep_hp(100.0), steep_lp(9000.0)].concat()),
    preset(
      tk("Male voice"),
      [steep_hp(80.0), vec![pk(140.0, 2.0, 1.0), pk(400.0, -2.5, 1.2), pk(3000.0, 3.0, 1.2)], steep_lp(12000.0)]
        .concat(),
    ),
    preset(
      tk("Female voice"),
      [steep_hp(120.0), vec![pk(250.0, -2.0, 1.2), pk(4500.0, 2.5, 1.2), pk(7500.0, -2.0, 3.0)], steep_lp(14000.0)]
        .concat(),
    ),
    preset(
      tk("Radio voice (deep, close)"),
      [
        steep_hp(70.0),
        vec![
          band(FilterType::LowShelf, 150.0, 4.0, 0.707),
          pk(450.0, -3.0, 1.0),
          pk(3000.0, 3.5, 1.0),
          band(FilterType::HighShelf, 9000.0, 1.5, 0.707),
        ],
        steep_lp(14000.0),
      ]
      .concat(),
    ),
    preset(
      tk("Discord clarity"),
      [steep_hp(100.0), vec![pk(300.0, -2.0, 1.0), pk(2500.0, 3.0, 1.2)], steep_lp(11000.0)].concat(),
    ),
    preset(
      tk("Tame harsh S sounds"),
      [steep_hp(90.0), vec![pk(6500.0, -5.0, 3.0), pk(9000.0, -3.0, 2.0)], steep_lp(13000.0)].concat(),
    ),
    preset(tk("Walkie-talkie (effect)"), [steep_hp(400.0), vec![pk(1500.0, 5.0, 1.0)], steep_lp(3000.0)].concat()),
    preset(tk("Flat (no EQ)"), Vec::new()),
  ]
}

fn user_dir(storage: &ProfileStorage) -> PathBuf {
  storage.base_dir().join(USER_DIR)
}

fn user_file(storage: &ProfileStorage, name: &str) -> PathBuf {
  user_dir(storage).join(format!("{}.json", ProfileStorage::sanitize_filename(name.trim())))
}

fn preset(name: &str, bands: Vec<BiquadBand>) -> ParametricProfile {
  let mut p = ParametricProfile::new(name, VadType::Mic);
  p.bands = bands;
  p.is_custom = false;
  p
}

fn steep_hp(freq: f32) -> Vec<BiquadBand> {
  vec![BiquadBand { slope_db: LOW_CUT_SLOPE_DB, ..band(FilterType::HighPass, freq, 0.0, 0.707) }]
}

fn steep_lp(freq: f32) -> Vec<BiquadBand> {
  vec![BiquadBand { slope_db: HIGH_CUT_SLOPE_DB, ..band(FilterType::LowPass, freq, 0.0, 0.707) }]
}

fn pk(freq: f32, gain_db: f32, q: f32) -> BiquadBand {
  band(FilterType::Peaking, freq, gain_db, q)
}

fn band(filter_type: FilterType, frequency: f32, gain_db: f32, q: f32) -> BiquadBand {
  BiquadBand { enabled: true, filter_type, frequency, gain_db, q, slope_db: 12 }
}

#[cfg(test)]
mod tests {
  use super::*;
  use gamersequalizer_common::biquad::MAX_EQ_BANDS;
  use gamersequalizer_common::profile::slots_used;

  #[test]
  fn presets_fit_the_band_limit_and_parameter_ranges() {
    for p in builtin_mic_presets() {
      assert!(slots_used(&p.bands) <= MAX_EQ_BANDS, "{}", p.name);
      for b in &p.bands {
        let mut clamped = b.clone();
        clamped.clamp_parameters();
        assert_eq!(&clamped, b, "{}", p.name);
      }
    }
    assert!(builtin_mic_presets().iter().any(|p| p.name == DEFAULT_PRESET));
  }

  #[test]
  fn modified_and_name_checks() {
    let mut list: Vec<MicPreset> =
      builtin_mic_presets().into_iter().map(|profile| MicPreset { source: PresetSource::Builtin, profile }).collect();
    let mut mine = ParametricProfile::new("Mine", VadType::Mic);
    list.push(MicPreset { source: PresetSource::User, profile: mine.clone() });

    assert!(!is_modified(&list, &mine));
    mine.bands.push(pk(1000.0, 1.0, 1.0));
    assert!(is_modified(&list, &mine));
    assert!(is_modified(&list, &ParametricProfile::new("Deleted meanwhile", VadType::Mic)));

    assert_eq!(check_new_name(&list, "  "), NameCheck::Empty);
    assert_eq!(check_new_name(&list, "Mine "), NameCheck::Overwrite);
    assert_eq!(check_new_name(&list, "MINE"), NameCheck::Overwrite);
    assert_eq!(check_new_name(&list, "Male voice"), NameCheck::Taken(PresetSource::Builtin));
    assert_eq!(check_new_name(&list, "New one"), NameCheck::Ok);
  }

  #[test]
  fn user_presets_save_list_first_and_delete() {
    let dir = std::env::temp_dir().join(format!("ge_mic_presets_{}", std::process::id()));
    let storage = ProfileStorage::new(&dir);
    let mut p = ParametricProfile::new("My voice", VadType::Mic);
    p.bands = steep_hp(90.0);
    save_user_preset(&storage, &p).unwrap();

    let list = all_mic_presets(&storage);
    assert_eq!(list[0].source, PresetSource::User);
    assert_eq!(list[0].profile.name, "My voice");
    assert_eq!(list[0].profile.bands, p.bands);

    delete_user_preset(&storage, "My voice").unwrap();
    assert!(all_mic_presets(&storage).iter().all(|p| p.source != PresetSource::User));
    let _ = std::fs::remove_dir_all(&dir);
  }
}
