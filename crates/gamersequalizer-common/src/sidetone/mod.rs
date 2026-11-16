//! Sidetone: the user's own voice in their headphones (sealed in-ears block it). This file
//! holds the parts that do not depend on Windows:
//!
//! mic APO: raw input -> `SidetoneChain` (EQ, band expander, level) -> `SidetoneRing` (in
//! the Global mapping) -> output APO: `SidetoneReader` -> mixed in before the output limiter.
//!
//! Nothing here adds delay: IIR filters and per-sample gains only, no lookahead. The delay of
//! the whole path is the audio engine's periods plus what `SidetoneReader` keeps queued so
//! that it never runs dry.
//!
//! `SidetonePairs` is the saved "microphone + output" matrix; the daemon owns its file.
//!
//! Limits:
//! - `SidetoneReader` has no anti-alias filter: a mic running at a higher rate than the
//!   output folds what is above the output's Nyquist back (a voice has little up there).
//! - A reader that runs dry cuts to silence at once (a click); it then keeps a larger margin.
//! - Microphone blocks of 100 ms or more look like a paused reader to `SidetoneReader` and
//!   are dropped (the audio engine's periods are 10 ms or less).
//! - The expander's bands overlap gently (differences of 12 dB/octave low-passes): a closed
//!   band takes up to ~1 dB from a tone in the band next to it.

use crate::biquad::BiquadCascade;
use crate::profile::{BiquadBand, FilterType};
use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

mod expander;
mod pairs;
mod ring;

pub use expander::BandExpander;
pub use pairs::{SidetonePair, SidetonePairs, default_bands};
pub use ring::{ReadInfo, SIDETONE_RING_LEN, SidetoneReader, SidetoneRing};

/// A sample beyond this is garbage (a torn write, a stale mapping): read as silence.
const SAMPLE_MAX: f32 = 4.0;

/// Top of the volume setting (300 %), like the microphone's "Sensitivity".
pub const VOLUME_MAX: f32 = 3.0;
/// Low cut of a new pair, Hz: the lows of one's own voice already arrive through the bones.
pub const GATE_MARGIN_DB_MIN: f32 = 3.0;
pub const GATE_MARGIN_DB_MAX: f32 = 20.0;
pub const GATE_RANGE_DB_MAX: f32 = 40.0;
/// The voice leveler's gain is followed only this far (its own range ends at +24 dB), so
/// garbage in the mapping cannot blast the headphones.
const LEVELER_GAIN_LIMIT_DB: f32 = 24.0;
/// The level moves to a new value over about this long (no zipper noise, no click on mute).
const LEVEL_RAMP_SECONDS: f32 = 0.010;

/// What suppresses noise in the sidetone. Crosses the shared memory as a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoiseMode {
  Off,
  /// Band expander in this file: no delay.
  #[default]
  Expander,
  /// WebRTC noise suppressor (`sonora`): frames, ~7 ms.
  WebRtcNs,
  /// RNNoise 2017 (`nnnoiseless`): frames, 10 ms.
  RnNoise,
  /// RNNoise 0.2 (`rnnoise2`): frames, 20 ms.
  RnNoise2,
}

/// What the user set for one microphone + output pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SidetoneSettings {
  pub enabled: bool,
  pub noise_mode: NoiseMode,
  /// Sidetone level, 1.0 = 100 %: the voice as loud as the voice leveler makes it for the
  /// others (the raw microphone while the leveler is off or has not learned yet).
  pub volume: f32,
  /// A band opens this many dB above its noise floor (sensitivity).
  pub gate_margin_db: f32,
  /// A closed band is turned down by this many dB (depth).
  pub gate_range_db: f32,
}

/// The sidetone processing in the mic APO, in place on the raw input (first channel):
/// `run_eq` -> noise suppression -> `run_level`. `process` does all three with the band
/// expander; the frame-based modes run their own suppressor between `run_eq` and
/// `run_level`.
#[derive(Debug, Clone)]
pub struct SidetoneChain {
  rate: f32,
  eq: BiquadCascade,
  expander: BandExpander,
  noise_mode: NoiseMode,
  level: f32,
  level_goal: f32,
  level_step: f32,
}

impl NoiseMode {
  pub fn to_u32(self) -> u32 {
    match self {
      NoiseMode::Off => 0,
      NoiseMode::Expander => 1,
      NoiseMode::WebRtcNs => 2,
      NoiseMode::RnNoise => 3,
      NoiseMode::RnNoise2 => 4,
    }
  }

  /// A mode this build does not know (settings from a newer one) runs as the expander.
  pub fn from_u32(v: u32) -> Self {
    match v {
      0 => NoiseMode::Off,
      2 => NoiseMode::WebRtcNs,
      3 => NoiseMode::RnNoise,
      4 => NoiseMode::RnNoise2,
      _ => NoiseMode::Expander,
    }
  }

  /// Works on 10 ms frames at 48 kHz, i.e. adds delay.
  pub fn is_framed(self) -> bool {
    matches!(self, NoiseMode::WebRtcNs | NoiseMode::RnNoise | NoiseMode::RnNoise2)
  }
}

impl Default for SidetoneSettings {
  fn default() -> Self {
    Self { enabled: false, noise_mode: NoiseMode::Expander, volume: 1.0, gate_margin_db: 10.0, gate_range_db: 18.0 }
  }
}

impl SidetoneSettings {
  /// Every value inside its range; a value that is not a number becomes the default.
  pub fn clamped(self) -> Self {
    let d = Self::default();
    let or = |v: f32, default: f32| if v.is_finite() { v } else { default };
    Self {
      enabled: self.enabled,
      noise_mode: self.noise_mode,
      volume: or(self.volume, d.volume).clamp(0.0, VOLUME_MAX),
      gate_margin_db: or(self.gate_margin_db, d.gate_margin_db).clamp(GATE_MARGIN_DB_MIN, GATE_MARGIN_DB_MAX),
      gate_range_db: or(self.gate_range_db, d.gate_range_db).clamp(0.0, GATE_RANGE_DB_MAX),
    }
  }
}

impl SidetoneChain {
  pub fn new(sample_rate: f32) -> Self {
    let rate = if sample_rate.is_finite() && sample_rate >= 8_000.0 { sample_rate } else { 48_000.0 };
    Self {
      rate,
      eq: BiquadCascade::default(),
      expander: BandExpander::new(rate),
      noise_mode: NoiseMode::Expander,
      level: 0.0,
      level_goal: 0.0,
      level_step: 1.0 / (LEVEL_RAMP_SECONDS * rate),
    }
  }

  /// The pair's EQ (`ShmHeader::read_sidetone_eq`). Recomputes the coefficients: call it only
  /// when the bands changed, like an EQ edit in the output APO.
  pub fn set_eq(&mut self, bands: impl IntoIterator<Item = impl Borrow<BiquadBand>>) {
    self.eq.set_bands(bands, self.rate);
  }

  /// Takes the settings over; cheap, no allocation. `leveler_gain_db` is the gain the voice
  /// leveler of the same mic APO applies now (0 while it is off or still learning):
  /// with it 100 % is the voice at the leveler's target, whatever the microphone's own gain,
  /// and the sidetone keeps its level when the Windows mic volume moves.
  pub fn configure(&mut self, settings: &SidetoneSettings, leveler_gain_db: f32, muted: bool) {
    let s = settings.clamped();
    self.expander.set(s.gate_margin_db, s.gate_range_db);
    self.noise_mode = s.noise_mode;
    let leveler_db = if leveler_gain_db.is_finite() { leveler_gain_db } else { 0.0 };
    let leveler = 10f32.powf(leveler_db.clamp(-LEVELER_GAIN_LIMIT_DB, LEVELER_GAIN_LIMIT_DB) / 20.0);
    self.level_goal = if s.enabled && !muted { s.volume * leveler } else { 0.0 };
  }

  pub fn noise_mode(&self) -> NoiseMode {
    self.noise_mode
  }

  /// Switched off or muted, and the level has reached zero: nothing to write to the ring.
  pub fn is_silent(&self) -> bool {
    self.level <= 0.0 && self.level_goal <= 0.0
  }

  pub fn expander_mut(&mut self) -> &mut BandExpander {
    &mut self.expander
  }

  /// Forgets filter memory and levels: for audio that does not continue the last block.
  pub fn reset(&mut self) {
    self.eq.reset_state();
    self.expander.reset();
    self.level = 0.0;
  }

  /// The whole chain with the band expander (or without, when the mode is not the expander).
  pub fn process(&mut self, samples: &mut [f32]) {
    self.run_eq(samples);
    if self.noise_mode == NoiseMode::Expander {
      self.expander.process(samples);
    }
    self.run_level(samples);
  }

  /// The pair's EQ. Garbage in the input (NaN, infinity) becomes silence first.
  pub fn run_eq(&mut self, samples: &mut [f32]) {
    for s in samples.iter_mut() {
      if !s.is_finite() {
        *s = 0.0;
      }
    }
    self.eq.process(samples);
  }

  /// Sidetone level, moved to its goal in small steps. The last stage: its output is finite.
  pub fn run_level(&mut self, samples: &mut [f32]) {
    for s in samples.iter_mut() {
      let diff = self.level_goal - self.level;
      // Relative steps would never leave zero; a fixed step of the larger of the two does.
      let step = self.level_step * self.level.max(self.level_goal).max(1e-3);
      self.level += diff.clamp(-step, step);
      let y = *s * self.level;
      *s = if y.is_finite() { y.clamp(-SAMPLE_MAX, SAMPLE_MAX) } else { 0.0 };
    }
  }
}

/// Stable 32-bit fingerprint of an endpoint id (FNV-1a over the lower-cased id), never 0:
/// 0 in the shared memory means "no device".
pub fn device_hash(id: &str) -> u32 {
  let hash = id.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b.to_ascii_lowercase())).wrapping_mul(0x0100_0193));
  hash.max(1)
}

#[cfg(test)]
mod tests;
