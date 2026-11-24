use serde::{Deserialize, Serialize};

/// Supported biquad filter types according to Equalizer APO and RBJ cookbook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterType {
  Peaking,
  LowShelf,
  HighShelf,
  LowPass,
  HighPass,
  Notch,
  /// A type from a newer version of the app: kept as written so a save does not destroy it,
  /// and passed through unchanged (it never reaches the audio engine).
  #[serde(untagged)]
  Unknown(String),
}

impl FilterType {
  /// Equalizer APO command abbreviation
  pub fn apo_abbreviation(&self) -> &'static str {
    match self {
      FilterType::Peaking => "PK",
      FilterType::LowShelf => "LSC",
      FilterType::HighShelf => "HSC",
      FilterType::LowPass => "LP",
      FilterType::HighPass => "HP",
      FilterType::Notch => "NO",
      FilterType::Unknown(_) => "",
    }
  }

  pub const ALL: [FilterType; 6] = [
    FilterType::Peaking,
    FilterType::LowShelf,
    FilterType::HighShelf,
    FilterType::LowPass,
    FilterType::HighPass,
    FilterType::Notch,
  ];

  /// Stable id for shared memory. Never renumber. An unknown type gets an id no reader knows,
  /// so the engine skips that band.
  pub fn to_u32(&self) -> u32 {
    match self {
      FilterType::Peaking => 0,
      FilterType::LowShelf => 1,
      FilterType::HighShelf => 2,
      FilterType::LowPass => 3,
      FilterType::HighPass => 4,
      FilterType::Notch => 5,
      FilterType::Unknown(_) => u32::MAX,
    }
  }

  pub fn from_u32(v: u32) -> Option<Self> {
    Self::ALL.into_iter().find(|t| t.to_u32() == v)
  }

  /// True when the gain parameter affects this filter type.
  pub fn uses_gain(&self) -> bool {
    matches!(self, FilterType::Peaking | FilterType::LowShelf | FilterType::HighShelf)
  }

  /// Display name for GUI
  pub fn display_name(&self) -> &'static str {
    match self {
      FilterType::Peaking => "Peaking (Bell)",
      FilterType::LowShelf => "Low Shelf",
      FilterType::HighShelf => "High Shelf",
      FilterType::LowPass => "Low Pass",
      FilterType::HighPass => "High Pass",
      FilterType::Notch => "Notch",
      FilterType::Unknown(_) => "Newer filter (passes unchanged)",
    }
  }
}

/// A single parametric EQ band
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BiquadBand {
  pub enabled: bool,
  pub filter_type: FilterType,
  pub frequency: f32, // 20.0 to 20000.0 Hz
  pub gain_db: f32,   // -24.0 to +24.0 dB (ignored for LP/HP/Notch)
  pub q: f32,         // 0.1 to 30.0
  /// Steepness of a low/high cut in dB per octave (12, 24 or 48). One biquad gives 12;
  /// steeper cuts are stacked Butterworth biquads, see `expand_slopes`. Ignored otherwise.
  #[serde(default = "default_slope")]
  pub slope_db: u8,
}

/// Cut steepness choices in dB per octave.
pub const SLOPES_DB: [u8; 3] = [12, 24, 48];
/// Butterworth Qs of the biquads that make a 4th order (24 dB/oct) and 8th order (48) cut.
const BUTTERWORTH_Q_24: [f32; 2] = [0.541, 1.307];
const BUTTERWORTH_Q_48: [f32; 4] = [0.510, 0.601, 0.900, 2.563];
/// Tolerances for recognising an already stacked cut (older presets saved it that way).
const STACK_Q_TOLERANCE: f32 = 0.02;
const STACK_FREQ_TOLERANCE_HZ: f32 = 0.5;

fn default_slope() -> u8 {
  12
}

impl Default for BiquadBand {
  fn default() -> Self {
    Self { enabled: true, filter_type: FilterType::Peaking, frequency: 1000.0, gain_db: 0.0, q: 1.414, slope_db: 12 }
  }
}

impl BiquadBand {
  pub fn clamp_parameters(&mut self) {
    self.frequency = self.frequency.clamp(20.0, 20000.0);
    self.gain_db = self.gain_db.clamp(-24.0, 24.0);
    self.q = self.q.clamp(0.1, 30.0);
    if !SLOPES_DB.contains(&self.slope_db) {
      self.slope_db = 12;
    }
  }

  /// Low cut (high-pass) or high cut (low-pass): the types that have a steepness.
  pub fn is_cut(&self) -> bool {
    matches!(self.filter_type, FilterType::HighPass | FilterType::LowPass)
  }

  /// How many biquads this band becomes in the audio engine.
  pub fn stages(&self) -> usize {
    self.butterworth_qs().map_or(1, |qs| qs.len())
  }

  pub(crate) fn butterworth_qs(&self) -> Option<&'static [f32]> {
    if !self.is_cut() {
      return None;
    }
    match self.slope_db {
      24 => Some(&BUTTERWORTH_Q_24),
      48 => Some(&BUTTERWORTH_Q_48),
      _ => None,
    }
  }
}

/// Biquads the audio engine runs: steep cuts become stacked 12 dB/oct stages, the rest is
/// copied. Idempotent (the output only has 12 dB/oct bands), so it is safe to apply twice.
pub fn expand_slopes(bands: &[BiquadBand]) -> Vec<BiquadBand> {
  let mut out = Vec::with_capacity(slots_used(bands));
  for band in bands {
    match band.butterworth_qs() {
      Some(qs) => out.extend(qs.iter().map(|&q| BiquadBand { q, slope_db: 12, ..band.clone() })),
      None => out.push(band.clone()),
    }
  }
  out
}

/// Biquads that undo `bands` exactly: peak and shelf stages with the gain negated (that swaps
/// numerator and denominator; `alpha` doesn't depend on the gain). Low/high-pass and notch
/// can't be undone (infinite gain where they cut) and are left out, so they still apply.
/// Disabled bands are dropped. Works on expanded bands, so steep cuts count per stage.
pub fn invert_bands(bands: &[BiquadBand]) -> Vec<BiquadBand> {
  expand_slopes(bands)
    .into_iter()
    .filter(|b| b.enabled && b.filter_type.uses_gain())
    .map(|b| BiquadBand { gain_db: -b.gain_db, ..b })
    .collect()
}

/// Engine slots (biquads) `bands` take, which is what the band limits count.
pub fn slots_used(bands: &[BiquadBand]) -> usize {
  bands.iter().map(BiquadBand::stages).sum()
}

/// Inverse of `expand_slopes`: neighbouring cuts with the Butterworth Qs of a 24/48 dB/oct
/// cut at the same frequency become one band with that slope (older saved presets).
pub fn collapse_slopes(bands: &[BiquadBand]) -> Vec<BiquadBand> {
  let mut out = Vec::with_capacity(bands.len());
  let mut i = 0;
  while let Some(first) = bands.get(i) {
    let stacked = [(48, &BUTTERWORTH_Q_48[..]), (24, &BUTTERWORTH_Q_24[..])].into_iter().find(|(_, qs)| {
      bands.get(i..i + qs.len()).is_some_and(|run| {
        run.iter().zip(qs.iter()).all(|(b, &q)| {
          b.is_cut()
            && b.slope_db == 12
            && b.filter_type == first.filter_type
            && b.enabled == first.enabled
            && (b.frequency - first.frequency).abs() <= STACK_FREQ_TOLERANCE_HZ
            && (b.q - q).abs() <= STACK_Q_TOLERANCE
        })
      })
    });
    match stacked {
      Some((slope_db, qs)) => {
        out.push(BiquadBand { slope_db, q: 0.707, ..first.clone() });
        i += qs.len();
      }
      None => {
        out.push(first.clone());
        i += 1;
      }
    }
  }
  out
}

/// Channel classification matching SteelSeries Sonar VADs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VadType {
  Game = 1,
  Chat = 2,
  Media = 3,
  Mic = 4,
  Aux = 5,
}

impl VadType {
  pub fn from_u32(val: u32) -> Option<Self> {
    match val {
      1 => Some(VadType::Game),
      2 => Some(VadType::Chat),
      3 => Some(VadType::Media),
      4 => Some(VadType::Mic),
      5 => Some(VadType::Aux),
      _ => None,
    }
  }

  pub fn as_str(&self) -> &'static str {
    match self {
      VadType::Game => "game",
      VadType::Chat => "chat",
      VadType::Media => "media",
      VadType::Mic => "mic",
      VadType::Aux => "aux",
    }
  }
}

/// Complete parametric EQ profile
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParametricProfile {
  pub name: String,
  pub vad_type: VadType,
  pub bands: Vec<BiquadBand>,
  pub preamp_db: f32,
  pub is_custom: bool,
  /// Launcher ids from the Sonar database ("1_2221490" = Steam app 2221490, "8_4932" =
  /// Ubisoft, "3_" GOG, "9_" Epic, "10_" Microsoft Store, other prefixes = launcher names).
  #[serde(default)]
  pub game_ids: Vec<String>,
  /// The user's volume trim for this profile in dB (added to the automatic preamp), so
  /// profiles can be matched by ear.
  #[serde(default)]
  pub gain_db: f32,
  /// The EQ as imported (AutoEQ, a pasted share text), for "Reset to original"; None for the
  /// user's own profiles, Sonar imports and profiles imported before this field existed.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub original: Option<OriginalEq>,
  /// Fields from a newer version of the app, kept so a save here does not drop them.
  #[serde(flatten)]
  pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Bands and preamp of a profile as it was imported. The user's volume trim (`gain_db`) is not
/// part of it: it is the user's own setting, not the imported correction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OriginalEq {
  pub bands: Vec<BiquadBand>,
  #[serde(default)]
  pub preamp_db: f32,
  /// Fields from a newer version of the app, kept so a save here does not drop them.
  #[serde(flatten)]
  pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ParametricProfile {
  pub fn new(name: impl Into<String>, vad_type: VadType) -> Self {
    Self {
      name: name.into(),
      vad_type,
      bands: Vec::new(),
      preamp_db: 0.0,
      is_custom: true,
      game_ids: Vec::new(),
      gain_db: 0.0,
      original: None,
      extra: serde_json::Map::new(),
    }
  }

  /// Remember the current bands and preamp as the imported original.
  pub fn remember_original(&mut self) {
    self.original =
      Some(OriginalEq { bands: self.bands.clone(), preamp_db: self.preamp_db, extra: serde_json::Map::new() });
  }

  /// True when the profile has an imported original and its bands or preamp differ from it.
  pub fn differs_from_original(&self) -> bool {
    self.original.as_ref().is_some_and(|o| o.bands != self.bands || o.preamp_db != self.preamp_db)
  }

  /// Put the imported bands and preamp back (the volume trim stays). False without an original.
  pub fn reset_to_original(&mut self) -> bool {
    let Some(o) = self.original.as_ref() else { return false };
    self.bands = o.bands.clone();
    self.preamp_db = o.preamp_db;
    true
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn original_eq_round_trips_and_old_files_have_none() {
    let old = r#"{"name": "HD 600", "vad_type": "media", "preamp_db": -6.0, "is_custom": false, "bands": []}"#;
    let profile: ParametricProfile = serde_json::from_str(old).unwrap();
    assert_eq!(profile.original, None);
    assert!(!profile.differs_from_original());
    assert!(!serde_json::to_string(&profile).unwrap().contains("original"), "None is not written");

    let newer = r#"{"name": "HD 600", "vad_type": "media", "preamp_db": -6.0, "is_custom": false,
      "bands": [{"enabled": true, "filter_type": "peaking", "frequency": 1000.0, "gain_db": 3.0, "q": 1.0}],
      "original": {"bands": [], "preamp_db": -6.0, "source": "autoeq"}}"#;
    let profile: ParametricProfile = serde_json::from_str(newer).unwrap();
    let saved = serde_json::to_string(&profile).unwrap();
    assert!(saved.contains(r#""source":"autoeq""#), "unknown fields of the original survive: {saved}");
    assert_eq!(serde_json::from_str::<ParametricProfile>(&saved).unwrap(), profile);
  }

  #[test]
  fn reset_to_original_restores_bands_and_preamp_but_keeps_the_volume_trim() {
    let mut p = ParametricProfile::new("HD 600", VadType::Media);
    p.bands = vec![BiquadBand { gain_db: 4.0, ..BiquadBand::default() }];
    p.preamp_db = -4.5;
    assert!(!p.reset_to_original(), "no original: nothing to reset to");
    p.remember_original();
    assert!(!p.differs_from_original());

    p.bands[0].frequency = 3000.0;
    p.gain_db = 2.0;
    assert!(p.differs_from_original());
    assert!(p.reset_to_original());
    assert!(!p.differs_from_original());
    assert_eq!(p.bands[0].frequency, BiquadBand::default().frequency);
    assert_eq!(p.gain_db, 2.0);
  }

  #[test]
  fn a_profile_from_a_newer_version_survives_load_and_save() {
    let newer = r#"{
      "name": "Future", "vad_type": "game", "preamp_db": -2.0, "is_custom": true, "spatial": {"room": 3},
      "bands": [
        {"enabled": true, "filter_type": "peaking", "frequency": 1000.0, "gain_db": 3.0, "q": 1.0},
        {"enabled": true, "filter_type": "all_pass", "frequency": 500.0, "gain_db": 0.0, "q": 0.7}
      ]
    }"#;
    let profile: ParametricProfile = serde_json::from_str(newer).unwrap();
    let unknown = &profile.bands[1].filter_type;
    assert_eq!(*unknown, FilterType::Unknown("all_pass".into()));
    // The engine never gets it: the shared-memory id is one no reader knows.
    assert_eq!(FilterType::from_u32(unknown.to_u32()), None);
    assert!(!unknown.uses_gain());

    let saved = serde_json::to_string(&profile).unwrap();
    let reloaded: ParametricProfile = serde_json::from_str(&saved).unwrap();
    assert_eq!(reloaded, profile);
    assert!(saved.contains(r#""filter_type":"all_pass""#) && saved.contains(r#""spatial":{"room":3}"#), "{saved}");
  }

  #[test]
  fn inverted_bands_cancel_peaks_and_shelves_exactly() {
    use crate::biquad::BiquadCascade;
    let band =
      |filter_type, frequency, gain_db, q| BiquadBand { filter_type, frequency, gain_db, q, ..BiquadBand::default() };
    let game = vec![
      band(FilterType::Peaking, 3000.0, 6.0, 1.4),
      band(FilterType::LowShelf, 120.0, -5.0, 0.7),
      band(FilterType::HighShelf, 9000.0, 4.0, 0.7),
      band(FilterType::HighPass, 30.0, 0.0, 0.7),
    ];
    let inverse = invert_bands(&game);
    assert_eq!(inverse.len(), 3, "the high-pass can't be undone");

    // White-ish noise through game then inverse == input (up to f32 coefficient rounding).
    let mut x = 1u32;
    let input: Vec<f32> = (0..48_000)
      .map(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (x >> 8) as f32 / (1u32 << 23) as f32 - 1.0
      })
      .collect();
    let peaks_only: Vec<BiquadBand> = game.iter().take(3).cloned().collect();
    let (mut forward, mut back) = (BiquadCascade::default(), BiquadCascade::default());
    forward.set_bands(&peaks_only, 48_000.0);
    back.set_bands(&inverse, 48_000.0);
    let mut buf = input.clone();
    forward.process(&mut buf);
    back.process(&mut buf);
    let err = buf.iter().zip(&input).fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
    assert!(err < 1e-3, "residual {err}");
    assert!(20.0 * err.max(1e-12).log10() < -60.0);
  }

  #[test]
  fn test_clamp_parameters() {
    let mut band = BiquadBand {
      enabled: true,
      filter_type: FilterType::Peaking,
      frequency: 10.0,
      gain_db: 30.0,
      q: 50.0,
      slope_db: 12,
    };
    band.clamp_parameters();
    assert_eq!(band.frequency, 20.0);
    assert_eq!(band.gain_db, 24.0);
    assert_eq!(band.q, 30.0);
  }

  #[test]
  fn steep_cuts_expand_to_stages_and_collapse_back() {
    let cut =
      |filter_type, slope_db| BiquadBand { filter_type, frequency: 80.0, q: 0.707, slope_db, ..BiquadBand::default() };
    let bands = vec![
      cut(FilterType::HighPass, 48),
      BiquadBand { gain_db: 3.0, ..BiquadBand::default() },
      cut(FilterType::LowPass, 24),
      cut(FilterType::LowPass, 12),
    ];
    let expanded = expand_slopes(&bands);
    assert_eq!(expanded.len(), 4 + 1 + 2 + 1);
    assert_eq!(slots_used(&bands), expanded.len());
    assert!(expanded.iter().all(|b| b.slope_db == 12));
    assert_eq!(expand_slopes(&expanded), expanded, "expanding twice changes nothing");
    assert_eq!(collapse_slopes(&expanded), bands);
  }

  #[test]
  fn old_slope_less_json_reads_as_12_db() {
    let band: BiquadBand =
      serde_json::from_str(r#"{"enabled":true,"filter_type":"high_pass","frequency":80.0,"gain_db":0.0,"q":0.7}"#)
        .unwrap_or_default();
    assert_eq!(band.slope_db, 12);
    assert_eq!(band.filter_type, FilterType::HighPass);
  }

  #[test]
  fn test_vad_type() {
    assert_eq!(VadType::from_u32(1), Some(VadType::Game));
    assert_eq!(VadType::from_u32(2), Some(VadType::Chat));
    assert_eq!(VadType::from_u32(99), None);
  }
}
