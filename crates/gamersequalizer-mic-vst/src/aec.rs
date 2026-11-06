//! NLMS acoustic echo canceller. The bulk delay is removed beforehand (timestamps +
//! `DelayEstimator`), so the filter only has to model the room response around it.

use gamersequalizer_common::shm::AEC_IMPULSE_POINTS;

/// 1024 taps = 21.3 ms at 48 kHz: from `PRE_DELAY_SAMPLES` before the measured delay to
/// ~18 ms after it (direct path + early reflections of a desk setup).
pub const DEFAULT_FILTER_TAPS: usize = 1024;
/// Filter starts this far before the measured delay so estimation error can't cut the
/// direct path off. Shows up as the impulse peak sitting at ~3 ms in the UI.
pub const PRE_DELAY_SAMPLES: usize = 144;
/// Geigel double-talk threshold: near-end speech when |mic| > T * max|ref| over the filter.
pub const DTD_THRESHOLD: f32 = 1.6;
/// Keep double-talk active 30 ms after the last trigger (standard hangover).
const DTD_HANGOVER_SAMPLES: u32 = 1440;
/// Reference power (mean square over the filter) under which nothing is adapted.
const MIN_REF_POWER: f64 = 1e-7;
/// Recompute the running reference energy exactly this often (kills float drift).
const ENERGY_RESYNC: u32 = 4096;

pub struct AcousticEchoCanceller {
  taps: usize,
  weights: Vec<f32>,
  /// Reference history stored twice so the last `taps` samples are one contiguous slice:
  /// `history[pos..pos + taps]` = x[n], x[n-1], ..., x[n-taps+1].
  history: Vec<f32>,
  pos: usize,
  ref_energy: f64,
  energy_counter: u32,
  step_size: f32,
  leakage: f32,
  erle_db: f32,
  mic_power_smooth: f32,
  err_power_smooth: f32,
  double_talk_active: bool,
  dtd_hold: u32,
}

impl AcousticEchoCanceller {
  pub fn new(taps: usize, step_size: f32) -> Self {
    let taps = taps.clamp(64, 4096);
    Self {
      taps,
      weights: vec![0.0; taps],
      history: vec![0.0; taps * 2],
      pos: 0,
      ref_energy: 0.0,
      energy_counter: 0,
      step_size: step_size.clamp(0.01, 1.0),
      leakage: 0.99999, // slight leakage prevents numerical drift
      erle_db: 0.0,
      mic_power_smooth: 1e-4,
      err_power_smooth: 1e-4,
      double_talk_active: false,
      dtd_hold: 0,
    }
  }

  pub fn default_48k() -> Self {
    Self::new(DEFAULT_FILTER_TAPS, 0.3)
  }

  pub fn taps(&self) -> usize {
    self.taps
  }

  /// Process one sample:
  /// `ref_sample`: reference from the speakers, already delay-aligned (minus pre-delay)
  /// `mic_sample`: raw microphone input containing near-end voice + speaker echo
  /// `max_ref`: max |ref| over the filter span (Geigel DTD), computed per block
  /// Returns cleaned sample `e(n) = mic - estimated_echo`
  #[inline]
  fn process_sample(&mut self, ref_sample: f32, mic_sample: f32, max_ref: f32) -> f32 {
    // Push newest sample at the front of the window (pointer moves backwards).
    self.pos = if self.pos == 0 { self.taps - 1 } else { self.pos - 1 };
    let leaving = self.history.get(self.pos).copied().unwrap_or(0.0);
    if let Some(slot) = self.history.get_mut(self.pos) {
      *slot = ref_sample;
    }
    if let Some(slot) = self.history.get_mut(self.pos + self.taps) {
      *slot = ref_sample;
    }
    self.ref_energy += (ref_sample as f64).powi(2) - (leaving as f64).powi(2);
    self.energy_counter += 1;

    let window = self.history.get(self.pos..self.pos + self.taps).unwrap_or(&[]);
    if self.energy_counter >= ENERGY_RESYNC {
      self.energy_counter = 0;
      self.ref_energy = window.iter().map(|&x| (x as f64).powi(2)).sum();
    }
    self.ref_energy = self.ref_energy.max(0.0);

    let echo_estimate: f32 = self.weights.iter().zip(window).map(|(w, x)| w * x).sum();
    let error = mic_sample - echo_estimate;

    // Double-Talk Detection (Geigel) with hangover
    let mic_mag = mic_sample.abs();
    if mic_mag > DTD_THRESHOLD * max_ref && mic_mag > 0.01 {
      self.dtd_hold = DTD_HANGOVER_SAMPLES;
    } else {
      self.dtd_hold = self.dtd_hold.saturating_sub(1);
    }
    self.double_talk_active = self.dtd_hold > 0;

    let ref_power = self.ref_energy / self.taps as f64;
    let far_end_active = ref_power > MIN_REF_POWER;

    // Adapt only on far-end-only signal
    if !self.double_talk_active && far_end_active {
      let update = (self.step_size as f64 / (self.ref_energy + 1e-3)) as f32 * error;
      let leak = self.leakage;
      for (w, &x) in self.weights.iter_mut().zip(window) {
        *w = *w * leak + update * x;
      }
    }

    // ERLE only means something while the far end plays and the user is quiet.
    if far_end_active && !self.double_talk_active {
      self.mic_power_smooth = 0.995 * self.mic_power_smooth + 0.005 * (mic_sample * mic_sample);
      self.err_power_smooth = 0.995 * self.err_power_smooth + 0.005 * (error * error);
      if self.mic_power_smooth > 1e-7 && self.err_power_smooth > 1e-10 {
        let db = 10.0 * (self.mic_power_smooth / self.err_power_smooth).log10();
        self.erle_db = 0.998 * self.erle_db + 0.002 * db.clamp(0.0, 60.0);
      }
    }

    // No non-linear residual suppression here: the old one attenuated the user's own
    // voice by up to 90 % whenever audio was playing. Residual echo is left to the
    // noise suppressor.
    error
  }

  /// Process a block in place. `ref_block` must be delay-aligned to `mic_block`.
  pub fn process_block(&mut self, ref_block: &[f32], mic_block: &mut [f32]) {
    // max|ref| over (filter span + this block), recomputed once per block.
    let span = self.history.get(self.pos..self.pos + self.taps).unwrap_or(&[]);
    let max_ref = span.iter().chain(ref_block.iter()).fold(1e-7f32, |m, x| m.max(x.abs()));

    for (m, &r) in mic_block.iter_mut().zip(ref_block) {
      *m = self.process_sample(r, *m, max_ref);
    }

    if self.weights.iter().any(|w| !w.is_finite()) {
      log::warn!("[AEC] filter diverged (non-finite weights), resetting");
      self.reset();
    }
  }

  pub fn erle_db(&self) -> f32 {
    self.erle_db
  }

  pub fn is_double_talk(&self) -> bool {
    self.double_talk_active
  }

  /// Max |w| per group of taps, oldest-delay first: the echo path impulse response.
  pub fn impulse_bins(&self, out: &mut [f32; AEC_IMPULSE_POINTS]) {
    let group = (self.taps / AEC_IMPULSE_POINTS).max(1);
    for (i, slot) in out.iter_mut().enumerate() {
      *slot = self
        .weights
        .get(i * group..((i + 1) * group).min(self.taps))
        .map_or(0.0, |g| g.iter().fold(0.0f32, |m, w| m.max(w.abs())));
    }
  }

  pub fn reset(&mut self) {
    self.weights.fill(0.0);
    self.history.fill(0.0);
    self.ref_energy = 0.0;
    self.erle_db = 0.0;
    self.mic_power_smooth = 1e-4;
    self.err_power_smooth = 1e-4;
    self.dtd_hold = 0;
    self.double_talk_active = false;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn noise(len: usize, seed: u32) -> Vec<f32> {
    let mut x = seed;
    (0..len)
      .map(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.5
      })
      .collect()
  }

  #[test]
  fn test_aec_cancels_delayed_echo_with_room_response() {
    let mut aec = AcousticEchoCanceller::new(256, 0.3);
    let speaker = noise(96_000, 11);
    // Echo path: direct path 40 samples late + a reflection at 120 samples.
    let echo: Vec<f32> = (0..speaker.len())
      .map(|n| {
        let a = if n >= 40 { speaker[n - 40] * 0.4 } else { 0.0 };
        let b = if n >= 120 { speaker[n - 120] * -0.15 } else { 0.0 };
        a + b
      })
      .collect();

    let mut before = 0.0f32;
    let mut after = 0.0f32;
    for (i, (r, m)) in speaker.chunks(480).zip(echo.chunks(480)).enumerate() {
      let mut out = m.to_vec();
      aec.process_block(r, &mut out);
      if i > 150 {
        before += m.iter().map(|x| x * x).sum::<f32>();
        after += out.iter().map(|x| x * x).sum::<f32>();
      }
    }
    let attenuation_db = 10.0 * (before / after).log10();
    assert!(attenuation_db > 20.0, "expected > 20 dB echo removal, got {attenuation_db:.1}");
    assert!(aec.erle_db() > 10.0);

    let mut ir = [0.0f32; AEC_IMPULSE_POINTS];
    aec.impulse_bins(&mut ir);
    let peak_bin = ir.iter().enumerate().fold((0, 0.0), |b, (i, &v)| if v > b.1 { (i, v) } else { b }).0;
    assert_eq!(peak_bin, 40 / (256 / AEC_IMPULSE_POINTS), "impulse peak must sit at the direct path");
  }

  #[test]
  fn test_double_talk_freezes_adaptation_and_keeps_voice() {
    let mut aec = AcousticEchoCanceller::new(128, 0.3);
    let speaker = noise(48_000, 5);
    for (r, chunk) in speaker.chunks(480).zip(speaker.chunks(480)) {
      let mut m: Vec<f32> = chunk.iter().map(|x| x * 0.3).collect();
      aec.process_block(r, &mut m);
    }

    // Loud near-end voice over quiet far end: DTD triggers and the voice passes.
    let r = vec![0.05f32; 480];
    let mut m: Vec<f32> = r.iter().map(|x| x * 0.3 + 0.8).collect();
    aec.process_block(&r, &mut m);
    assert!(aec.is_double_talk());
    let voice_level = m.iter().map(|x| x.abs()).sum::<f32>() / m.len() as f32;
    assert!(voice_level > 0.7, "near-end voice must not be suppressed, got {voice_level}");
  }
}
