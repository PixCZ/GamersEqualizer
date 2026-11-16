//! The echo strength, measured next to AEC3 from the reference and the mic level per window.

use super::*;

/// Echo strength is measured in windows of this many frames (100 ms)...
const ECHO_WINDOW_FRAMES: u32 = 10;
/// ...in which the reference is this loud (mean square; -30 dBFS).
const REF_LOUD_POWER: f32 = 1e-3;
/// The last such windows kept (12.8 s), and how many it takes for a first estimate (5 s).
const ECHO_WINDOWS: usize = 128;
pub(super) const ECHO_MIN_WINDOWS: usize = 50;
/// An echo is confirmed when the mic's level follows the reference's in the windows quietest
/// for their reference: correlation of the levels at least this, a slope near 1...
const ECHO_FIT_MIN_CORRELATION: f32 = 0.9;
const ECHO_FIT_SLOPE: std::ops::RangeInclusive<f32> = 0.7..=1.3;
/// ...and a reference whose level moves at least this much in them (standard deviation, dB):
/// a level that stands still shows nothing.
const ECHO_FIT_MIN_REF_SPREAD_DB: f32 = 2.0;
/// A confirmed strength comes down when the bound falls this far under it (the speakers were
/// turned down, headphones went on).
const ECHO_BOUND_UNDER_DB: f32 = 3.0;
/// The reported echo strength moves only by this much (dB), so a log shows steps, not jitter.
const ECHO_REPORT_STEP_DB: f32 = 2.0;

/// Echo strength: how many dB under the reference its echo reaches the raw mic. Per 100 ms
/// window with a loud reference: mic level - reference level.
///
/// The user's voice and the mic's noise only ever add to a window, so a low quartile of the
/// last windows is a bound: the echo is no stronger than that. It is not the echo: on the
/// captures a voice over the music read as an echo 12-30 dB under it (milestone 46: headphones
/// with no echo at all "-31 dB", speech over a -50 dB echo "-33 dB").
///
/// An echo is told from a voice by following the reference: where the echo is what the mic
/// hears, the mic's level goes up and down with the reference's, window by window; a voice or
/// a noise does not. Only then is the strength confirmed, and only a confirmed one moves
/// AEC3's fallback. Not confirmed, the meter says "a bound" and AEC3 keeps its lowest
/// fallback, as before there was a meter.
pub(super) struct EchoMeter {
  /// The window being filled: reference energy, mic energy, frames.
  window: (f32, f32, u32),
  /// (reference level, mic level) in dB of the last loud-reference windows.
  loud: [(f32, f32); ECHO_WINDOWS],
  pub(super) loud_count: usize,
  /// The strength last confirmed or remembered, brought down by a lower bound since.
  pub(super) confirmed: Option<f32>,
  /// What is reported (see `ECHO_REPORT_STEP_DB`): the confirmed strength, else the bound.
  pub(super) shown: Option<(f32, bool)>,
  /// Correlation and slope of the last fit: for the log, where they show how near a real
  /// room comes to `ECHO_FIT_MIN_CORRELATION`.
  pub(super) fit: (f32, f32),
  /// `ECHO_FIT_MIN_CORRELATION`; the capture report tries others.
  pub(super) min_correlation: f32,
}

impl EchoMeter {
  pub(super) fn new() -> Self {
    Self {
      window: (0.0, 0.0, 0),
      loud: [(0.0, 0.0); ECHO_WINDOWS],
      loud_count: 0,
      confirmed: None,
      shown: None,
      fit: (0.0, 0.0),
      min_correlation: ECHO_FIT_MIN_CORRELATION,
    }
  }

  /// One frame of the reference as AEC3 gets it and of the raw mic.
  pub(super) fn push(&mut self, refs: &[f32], mic: &[f32]) {
    let energy = |x: &[f32]| x.iter().map(|s| s * s).sum::<f32>();
    self.window = (self.window.0 + energy(refs), self.window.1 + energy(mic), self.window.2 + 1);
    if self.window.2 < ECHO_WINDOW_FRAMES {
      return;
    }
    let samples = (self.window.2 as usize * FRAME) as f32;
    let (ref_power, mic_power) = (self.window.0 / samples, self.window.1 / samples);
    self.window = (0.0, 0.0, 0);
    if ref_power >= REF_LOUD_POWER {
      if let Some(slot) = self.loud.get_mut(self.loud_count % ECHO_WINDOWS) {
        *slot = (10.0 * ref_power.log10(), 10.0 * mic_power.max(1e-12).log10());
      }
      self.loud_count += 1;
      self.estimate();
    }
  }

  fn estimate(&mut self) {
    let filled = self.loud_count.min(ECHO_WINDOWS);
    if filled < ECHO_MIN_WINDOWS {
      return;
    }
    // (mic level - reference level, reference level), the quietest for its reference first.
    let mut sorted = self.loud.map(|(ref_db, mic_db)| (mic_db - ref_db, ref_db));
    let sorted = sorted.get_mut(..filled).unwrap_or_default();
    sorted.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let Some(&(bound, _)) = sorted.get(filled / 4) else { return };

    // Does the mic follow the reference in the quieter half (the windows a voice is least in)?
    let lower = sorted.get(..filled / 2).unwrap_or_default();
    let n = lower.len() as f32;
    let ref_mean = lower.iter().map(|w| w.1).sum::<f32>() / n;
    let mic_mean = lower.iter().map(|w| w.0 + w.1).sum::<f32>() / n;
    let (mut ref_var, mut mic_var, mut cov) = (0.0f32, 0.0f32, 0.0f32);
    for &(ratio, ref_db) in lower {
      let (r, m) = (ref_db - ref_mean, ratio + ref_db - mic_mean);
      (ref_var, mic_var, cov) = (ref_var + r * r / n, mic_var + m * m / n, cov + r * m / n);
    }
    let correlation = cov / (ref_var * mic_var).sqrt().max(1e-9);
    let slope = cov / ref_var.max(1e-9);
    self.fit = (correlation, slope);
    let follows = ref_var.sqrt() >= ECHO_FIT_MIN_REF_SPREAD_DB
      && correlation >= self.min_correlation
      && ECHO_FIT_SLOPE.contains(&slope);
    if follows || self.confirmed.is_some_and(|c| bound < c - ECHO_BOUND_UNDER_DB) {
      self.confirmed = Some(bound);
    }
    let now = self.confirmed.map_or((bound, true), |c| (c, false));
    if self.shown.is_none_or(|was| (was.0 - now.0).abs() >= ECHO_REPORT_STEP_DB || was.1 != now.1) {
      self.shown = Some(now);
    }
  }
}
