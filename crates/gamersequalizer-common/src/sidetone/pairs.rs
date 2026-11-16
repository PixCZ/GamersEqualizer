//! The saved "microphone + output" pairs (`profiles/sidetone.json`); the daemon owns the file.

use super::*;

const DEFAULT_LOW_CUT_HZ: f32 = 200.0;
/// One saved pair. Unknown JSON fields are ignored, missing ones take the defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidetonePair {
  pub mic_id: String,
  pub output_id: String,
  /// Names as shown when the pair was saved (for the list; the ids decide).
  pub mic_name: String,
  pub output_name: String,
  pub enabled: bool,
  /// `NoiseMode::to_u32`; a mode from a newer version stays as its number.
  pub noise_mode: u32,
  pub volume: f32,
  pub gate_margin_db: f32,
  pub gate_range_db: f32,
  /// Parametric EQ of the sidetone, the same bands as the voice EQ (`write_sidetone_eq`).
  pub bands: Vec<BiquadBand>,
}

/// The saved matrix (`profiles/sidetone.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidetonePairs {
  pub pairs: Vec<SidetonePair>,
}

impl Default for SidetonePair {
  fn default() -> Self {
    Self::new("", "", "", "")
  }
}

impl SidetonePair {
  /// A new pair with the default settings, switched off.
  pub fn new(mic_id: &str, output_id: &str, mic_name: &str, output_name: &str) -> Self {
    let s = SidetoneSettings::default();
    Self {
      mic_id: mic_id.to_string(),
      output_id: output_id.to_string(),
      mic_name: mic_name.to_string(),
      output_name: output_name.to_string(),
      enabled: s.enabled,
      noise_mode: s.noise_mode.to_u32(),
      volume: s.volume,
      gate_margin_db: s.gate_margin_db,
      gate_range_db: s.gate_range_db,
      bands: default_bands(),
    }
  }

  pub fn settings(&self) -> SidetoneSettings {
    SidetoneSettings {
      enabled: self.enabled,
      noise_mode: NoiseMode::from_u32(self.noise_mode),
      volume: self.volume,
      gate_margin_db: self.gate_margin_db,
      gate_range_db: self.gate_range_db,
    }
    .clamped()
  }

  /// Takes the settings over; the ids and names stay.
  pub fn set_settings(&mut self, s: &SidetoneSettings) {
    let s = s.clamped();
    self.enabled = s.enabled;
    self.noise_mode = s.noise_mode.to_u32();
    self.volume = s.volume;
    self.gate_margin_db = s.gate_margin_db;
    self.gate_range_db = s.gate_range_db;
  }

  fn is(&self, mic_id: &str, output_id: &str) -> bool {
    self.mic_id.eq_ignore_ascii_case(mic_id) && self.output_id.eq_ignore_ascii_case(output_id)
  }
}

impl SidetonePairs {
  pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
    serde_json::from_str(text)
  }

  pub fn to_json(&self) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(self)
  }

  pub fn find(&self, mic_id: &str, output_id: &str) -> Option<&SidetonePair> {
    self.pairs.iter().find(|p| p.is(mic_id, output_id))
  }

  /// Replaces the pair with the same ids, or adds it.
  pub fn upsert(&mut self, pair: SidetonePair) {
    match self.pairs.iter_mut().find(|p| p.is(&pair.mic_id, &pair.output_id)) {
      Some(slot) => *slot = pair,
      None => self.pairs.push(pair),
    }
  }

  pub fn remove(&mut self, mic_id: &str, output_id: &str) {
    self.pairs.retain(|p| !p.is(mic_id, output_id));
  }
}

/// EQ of a new pair: only the low cut.
pub fn default_bands() -> Vec<BiquadBand> {
  vec![BiquadBand {
    enabled: true,
    filter_type: FilterType::HighPass,
    frequency: DEFAULT_LOW_CUT_HZ,
    gain_db: 0.0,
    q: std::f32::consts::FRAC_1_SQRT_2,
    slope_db: 12,
  }]
}
