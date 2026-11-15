use crate::profile::{BiquadBand, FilterType};
use std::borrow::Borrow;
use std::f32::consts::PI;

/// Upper bound of microphone EQ bands (also its shared-memory slot count).
pub const MAX_EQ_BANDS: usize = 16;
/// Output EQ: headphone correction (AutoEQ uses up to 10) + game profile (Sonar up to 10).
pub const MAX_OUT_EQ_BANDS: usize = 32;
/// Biquads of one stream EQ (chat/media profile + the inverted game profile); also the
/// most stages a cascade runs.
pub const MAX_STREAM_EQ_BANDS: usize = 64;
/// Slots one output layer (headphones, game, chat, media) may use in its editor: two layers
/// share `MAX_OUT_EQ_BANDS`.
pub const MAX_LAYER_EQ_BANDS: usize = MAX_OUT_EQ_BANDS / 2;

#[derive(Debug, Clone, Copy)]
pub struct BiquadCoefficients {
  pub b0: f32,
  pub b1: f32,
  pub b2: f32,
  pub a0: f32,
  pub a1: f32,
  pub a2: f32,
}

/// Cascade of biquads processed in Transposed Direct Form II (f64 state and f64 between
/// stages: avoids SSE2 f32<->f64 round-trips and intermediate quantization noise).
#[derive(Debug, Clone)]
pub struct BiquadCascade {
  channels: usize,
  coefs: Vec<StageCoefs>,
  state: FilterState,
}

impl Default for BiquadCascade {
  fn default() -> Self {
    Self::with_channels(1)
  }
}

/// Per-stage, per-channel filter memory (`stage_idx * channels + ch`), kept in separate
/// slices (SoA) so stereo and 5.1/7.1 vectorize cleanly and use FMA when available.
#[derive(Debug, Clone, Default)]
struct FilterState {
  z1: Vec<f64>,
  z2: Vec<f64>,
}

impl FilterState {
  fn reserve(&mut self, len: usize) {
    self.z1.reserve(len.saturating_sub(self.z1.len()));
    self.z2.reserve(len.saturating_sub(self.z2.len()));
  }

  fn resize_and_zero(&mut self, len: usize) {
    self.z1.resize(len, 0.0);
    self.z2.resize(len, 0.0);
    self.reset();
  }

  fn reset(&mut self) {
    self.z1.fill(0.0);
    self.z2.fill(0.0);
  }

  fn clear(&mut self) {
    self.z1.clear();
    self.z2.clear();
  }

  fn flush_non_finite(&mut self) {
    // Recover from NaN/Inf so one bad block cannot poison the filter state.
    for (s1, s2) in self.z1.iter_mut().zip(self.z2.iter_mut()) {
      if !s1.is_finite() || !s2.is_finite() {
        *s1 = 0.0;
        *s2 = 0.0;
      }
    }
  }
}

#[derive(Debug, Clone, Copy)]
struct StageCoefs {
  b0: f64,
  b1: f64,
  b2: f64,
  a1: f64,
  a2: f64,
}

impl BiquadCoefficients {
  /// Calculate biquad coefficients using RBJ Audio-EQ-Cookbook formulas. Total: any input
  /// (profiles from disk, a bogus stream rate) gives finite coefficients, never a panic
  /// (`clamp` panics when min > max) and never NaN (which `process_strided` can't recover from,
  /// it only resets the state).
  pub fn from_band(band: &BiquadBand, sample_rate: f32) -> Self {
    let sample_rate = if sample_rate.is_finite() && sample_rate >= 8000.0 { sample_rate } else { 48_000.0 };
    let f0 = if band.frequency.is_finite() { band.frequency.max(20.0).min(sample_rate * 0.499) } else { 1000.0 };
    let q = if band.q.is_finite() { band.q.clamp(0.1, 30.0) } else { std::f32::consts::FRAC_1_SQRT_2 };
    // +-60 dB is far past any real EQ setting; above ~770 dB a shelf overflows f32.
    let gain_db = if band.gain_db.is_finite() { band.gain_db.clamp(-60.0, 60.0) } else { 0.0 };

    let w0 = 2.0 * PI * f0 / sample_rate;
    let cos_w0 = w0.cos();
    let sin_w0 = w0.sin();
    let alpha = sin_w0 / (2.0 * q);
    let a_linear = 10.0f32.powf(gain_db / 40.0); // sqrt(10^(gain/20))

    let (b0, b1, b2, a0, a1, a2) = match band.filter_type {
      FilterType::Peaking => {
        let b0 = 1.0 + alpha * a_linear;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0 - alpha * a_linear;
        let a0 = 1.0 + alpha / a_linear;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha / a_linear;
        (b0, b1, b2, a0, a1, a2)
      }
      FilterType::LowShelf => {
        let sqrt_a = a_linear.sqrt();
        let b0 = a_linear * ((a_linear + 1.0) - (a_linear - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha);
        let b1 = 2.0 * a_linear * ((a_linear - 1.0) - (a_linear + 1.0) * cos_w0);
        let b2 = a_linear * ((a_linear + 1.0) - (a_linear - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha);
        let a0 = (a_linear + 1.0) + (a_linear - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha;
        let a1 = -2.0 * ((a_linear - 1.0) + (a_linear + 1.0) * cos_w0);
        let a2 = (a_linear + 1.0) + (a_linear - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha;
        (b0, b1, b2, a0, a1, a2)
      }
      FilterType::HighShelf => {
        let sqrt_a = a_linear.sqrt();
        let b0 = a_linear * ((a_linear + 1.0) + (a_linear - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha);
        let b1 = -2.0 * a_linear * ((a_linear - 1.0) + (a_linear + 1.0) * cos_w0);
        let b2 = a_linear * ((a_linear + 1.0) + (a_linear - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha);
        let a0 = (a_linear + 1.0) - (a_linear - 1.0) * cos_w0 + 2.0 * sqrt_a * alpha;
        let a1 = 2.0 * ((a_linear - 1.0) - (a_linear + 1.0) * cos_w0);
        let a2 = (a_linear + 1.0) - (a_linear - 1.0) * cos_w0 - 2.0 * sqrt_a * alpha;
        (b0, b1, b2, a0, a1, a2)
      }
      FilterType::LowPass => {
        let b0 = (1.0 - cos_w0) * 0.5;
        let b1 = 1.0 - cos_w0;
        let b2 = (1.0 - cos_w0) * 0.5;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;
        (b0, b1, b2, a0, a1, a2)
      }
      FilterType::HighPass => {
        let b0 = (1.0 + cos_w0) * 0.5;
        let b1 = -(1.0 + cos_w0);
        let b2 = (1.0 + cos_w0) * 0.5;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;
        (b0, b1, b2, a0, a1, a2)
      }
      FilterType::Notch => {
        let b0 = 1.0;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;
        (b0, b1, b2, a0, a1, a2)
      }
    };

    let c = Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a0: 1.0, a1: a1 / a0, a2: a2 / a0 };
    if [c.b0, c.b1, c.b2, c.a1, c.a2].iter().all(|v| v.is_finite()) {
      c
    } else {
      // Pass-through rather than a filter that turns every later block into NaN.
      Self { b0: 1.0, b1: 0.0, b2: 0.0, a0: 1.0, a1: 0.0, a2: 0.0 }
    }
  }

  /// Complex frequency response magnitude in decibels at frequency f
  pub fn response_db_at(&self, freq: f32, sample_rate: f32) -> f32 {
    let w = 2.0 * PI * freq / sample_rate;
    let cos_w = w.cos();
    let sin_w = w.sin();
    let cos_2w = (2.0 * w).cos();
    let sin_2w = (2.0 * w).sin();

    // Numerator: b0 + b1*e^(-jw) + b2*e^(-j2w)
    let num_re = self.b0 + self.b1 * cos_w + self.b2 * cos_2w;
    let num_im = -self.b1 * sin_w - self.b2 * sin_2w;
    let num_mag_sq = num_re * num_re + num_im * num_im;

    // Denominator: 1 + a1*e^(-jw) + a2*e^(-j2w)
    let den_re = 1.0 + self.a1 * cos_w + self.a2 * cos_2w;
    let den_im = -self.a1 * sin_w - self.a2 * sin_2w;
    let den_mag_sq = den_re * den_re + den_im * den_im;

    if den_mag_sq < 1e-12 {
      return 0.0;
    }

    10.0 * (num_mag_sq / den_mag_sq).log10()
  }
}

impl BiquadCascade {
  pub fn with_channels(channels: usize) -> Self {
    // Room for the most stages up front: `set_bands` then never allocates on the audio thread.
    let channels = channels.max(1);
    let mut cascade = Self { channels, coefs: Vec::with_capacity(MAX_STREAM_EQ_BANDS), state: FilterState::default() };
    cascade.state.reserve(MAX_STREAM_EQ_BANDS * channels);
    cascade
  }

  /// Resize per-channel filter state when the stream channel count changes (resets memory).
  pub fn set_channels(&mut self, channels: usize) {
    let channels = channels.max(1);
    if channels != self.channels {
      self.channels = channels;
      // Allocates when the channel count grows (a new stream format, not every block).
      self.state.reserve(MAX_STREAM_EQ_BANDS * channels);
      self.state.resize_and_zero(self.coefs.len() * channels);
    }
  }

  /// Rebuild coefficients for the enabled bands (steep cuts as stacked stages, like
  /// `expand_slopes`). Filter state is kept when the stage count is unchanged, so dragging a
  /// slider doesn't click. No allocation here: the APOs call it on the audio thread, straight
  /// from the shared-memory readers (`ShmHeader::read_out_eq` and the like).
  pub fn set_bands(&mut self, bands: impl IntoIterator<Item = impl Borrow<BiquadBand>>, sample_rate: f32) {
    let old_len = self.coefs.len();
    self.coefs.clear();
    for band in bands {
      let band = band.borrow();
      if !band.enabled {
        continue;
      }
      let stage = |b: &BiquadBand| {
        let c = BiquadCoefficients::from_band(b, sample_rate);
        StageCoefs { b0: c.b0 as f64, b1: c.b1 as f64, b2: c.b2 as f64, a1: c.a1 as f64, a2: c.a2 as f64 }
      };
      let room = MAX_STREAM_EQ_BANDS.saturating_sub(self.coefs.len());
      match band.butterworth_qs() {
        Some(qs) => {
          self.coefs.extend(qs.iter().take(room).map(|&q| stage(&BiquadBand { q, slope_db: 12, ..band.clone() })))
        }
        None if room > 0 => self.coefs.push(stage(band)),
        None => {}
      }
    }
    if self.coefs.len() != old_len {
      self.state.resize_and_zero(self.coefs.len() * self.channels);
    }
  }

  pub fn clear(&mut self) {
    self.coefs.clear();
    self.state.clear();
  }

  pub fn is_empty(&self) -> bool {
    self.coefs.is_empty()
  }

  /// Forget the filter memory (coefficients stay): for audio that does not continue the
  /// samples processed last (resume after a pause).
  pub fn reset_state(&mut self) {
    self.state.reset();
  }

  pub fn process(&mut self, samples: &mut [f32]) {
    self.process_strided(samples, 1, 0);
  }

  /// Process one channel of an interleaved buffer (`channel` of `channels`). A one-channel
  /// cascade may run any channel (one cascade per channel); a multichannel one only its own
  /// channels, anything else is left untouched instead of sharing another channel's memory.
  pub fn process_strided(&mut self, samples: &mut [f32], channels: usize, channel: usize) {
    if self.coefs.is_empty() {
      return;
    }
    let step = channels.max(1);
    if self.channels == 1 && step == 1 && channel == 0 {
      run_lanes::<1>(&self.coefs, &mut self.state, samples);
    } else {
      let slot = match self.channels {
        1 => 0,
        n if channel < n => channel,
        _ => return,
      };
      run_channel(&self.coefs, &mut self.state, self.channels, slot, samples, step, channel);
    }
    self.state.flush_non_finite();
  }

  /// Process all channels of an interleaved buffer in one cascade (shared coefficients,
  /// per-channel filter memory). Mono, stereo, quad, 5.1, and 7.1 run in a single pass.
  pub fn process_interleaved(&mut self, samples: &mut [f32], channels: usize) {
    if self.coefs.is_empty() || channels == 0 {
      return;
    }
    self.set_channels(channels);
    match channels {
      1 => run_lanes::<1>(&self.coefs, &mut self.state, samples),
      2 => run_lanes::<2>(&self.coefs, &mut self.state, samples),
      4 => run_lanes::<4>(&self.coefs, &mut self.state, samples),
      6 => run_lanes::<6>(&self.coefs, &mut self.state, samples),
      8 => run_lanes::<8>(&self.coefs, &mut self.state, samples),
      _ => {
        for ch in 0..channels {
          run_channel(&self.coefs, &mut self.state, channels, ch, samples, channels, ch);
        }
      }
    }
    self.state.flush_non_finite();
  }
}

fn run_lanes<const CH: usize>(coefs: &[StageCoefs], state: &mut FilterState, samples: &mut [f32]) {
  #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
  if std::arch::is_x86_feature_detected!("fma") {
    // SAFETY: guarded by runtime CPU feature detection (`is_x86_feature_detected!("fma")`).
    unsafe { sample_outer_lanes_fma::<CH>(coefs, &mut state.z1, &mut state.z2, samples) };
    return;
  }
  sample_outer_lanes::<CH, false>(coefs, &mut state.z1, &mut state.z2, samples);
}

fn run_channel(
  coefs: &[StageCoefs],
  state: &mut FilterState,
  state_stride: usize,
  state_slot: usize,
  samples: &mut [f32],
  sample_stride: usize,
  sample_channel: usize,
) {
  #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
  if std::arch::is_x86_feature_detected!("fma") {
    // SAFETY: guarded by runtime CPU feature detection (`is_x86_feature_detected!("fma")`).
    unsafe {
      sample_outer_channel_fma(coefs, state, state_stride, state_slot, samples, sample_stride, sample_channel);
    }
    return;
  }
  sample_outer_channel::<false>(coefs, state, state_stride, state_slot, samples, sample_stride, sample_channel);
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "fma")]
fn sample_outer_lanes_fma<const CH: usize>(coefs: &[StageCoefs], z1: &mut [f64], z2: &mut [f64], samples: &mut [f32]) {
  sample_outer_lanes::<CH, true>(coefs, z1, z2, samples);
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "fma")]
fn sample_outer_channel_fma(
  coefs: &[StageCoefs],
  state: &mut FilterState,
  state_stride: usize,
  state_slot: usize,
  samples: &mut [f32],
  sample_stride: usize,
  sample_channel: usize,
) {
  sample_outer_channel::<true>(coefs, state, state_stride, state_slot, samples, sample_stride, sample_channel);
}

#[inline(always)]
fn mul_add_f64<const FMA: bool>(a: f64, b: f64, c: f64) -> f64 {
  if FMA { a.mul_add(b, c) } else { a * b + c }
}

#[inline(always)]
fn step_biquad<const FMA: bool>(c: &StageCoefs, z1: &mut f64, z2: &mut f64, x: f64) -> f64 {
  let y = mul_add_f64::<FMA>(c.b0, x, *z1);
  let a1_term = mul_add_f64::<FMA>(-c.a1, y, *z2);
  *z1 = mul_add_f64::<FMA>(c.b1, x, a1_term);
  *z2 = mul_add_f64::<FMA>(c.b2, x, -c.a2 * y);
  y
}

#[inline(always)]
fn sample_outer_channel<const FMA: bool>(
  coefs: &[StageCoefs],
  state: &mut FilterState,
  state_stride: usize,
  state_slot: usize,
  samples: &mut [f32],
  sample_stride: usize,
  sample_channel: usize,
) {
  for s in samples.iter_mut().skip(sample_channel).step_by(sample_stride) {
    let mut x = *s as f64;
    for (c, (s1, s2)) in coefs.iter().zip(
      state
        .z1
        .iter_mut()
        .skip(state_slot)
        .step_by(state_stride)
        .zip(state.z2.iter_mut().skip(state_slot).step_by(state_stride)),
    ) {
      x = step_biquad::<FMA>(c, s1, s2, x);
    }
    *s = x as f32;
  }
}

#[inline(always)]
fn sample_outer_lanes<const CH: usize, const FMA: bool>(
  coefs: &[StageCoefs],
  z1: &mut [f64],
  z2: &mut [f64],
  samples: &mut [f32],
) {
  let (s1_stages, _) = z1.as_chunks_mut::<CH>();
  let (s2_stages, _) = z2.as_chunks_mut::<CH>();
  for frame in samples.as_chunks_mut::<CH>().0 {
    let mut x = [0.0f64; CH];
    for (dst, src) in x.iter_mut().zip(frame.iter()) {
      *dst = *src as f64;
    }
    for ((c, s1), s2) in coefs.iter().zip(s1_stages.iter_mut()).zip(s2_stages.iter_mut()) {
      for ((xv, z1v), z2v) in x.iter_mut().zip(s1.iter_mut()).zip(s2.iter_mut()) {
        *xv = step_biquad::<FMA>(c, z1v, z2v, *xv);
      }
    }
    for (dst, src) in frame.iter_mut().zip(x.iter()) {
      *dst = *src as f32;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::profile::expand_slopes;

  #[test]
  fn set_bands_builds_the_expanded_stages_in_place() {
    let band = |filter_type, frequency, slope_db, enabled| BiquadBand {
      enabled,
      filter_type,
      frequency,
      gain_db: 3.0,
      q: 0.9,
      slope_db,
    };
    let bands = [
      band(FilterType::HighPass, 80.0, 48, true),
      band(FilterType::Peaking, 1000.0, 12, true),
      band(FilterType::Peaking, 2000.0, 12, false),
      band(FilterType::LowPass, 9000.0, 24, true),
    ];
    let mut cascade = BiquadCascade::default();
    let capacity = cascade.coefs.capacity();
    cascade.set_bands(&bands, 48_000.0);
    let expected: Vec<_> = expand_slopes(&bands).into_iter().filter(|b| b.enabled).collect();
    assert_eq!(cascade.coefs.len(), expected.len());
    for (got, b) in cascade.coefs.iter().zip(&expected) {
      let c = BiquadCoefficients::from_band(b, 48_000.0);
      assert_eq!((got.b0, got.a1, got.a2), (c.b0 as f64, c.a1 as f64, c.a2 as f64));
    }
    // More bands than stages: capped, and still no new allocation.
    cascade.set_bands(vec![band(FilterType::HighPass, 80.0, 48, true); 40], 48_000.0);
    assert_eq!(cascade.coefs.len(), MAX_STREAM_EQ_BANDS);
    assert_eq!(cascade.coefs.capacity(), capacity);
  }

  #[test]
  fn multichannel_cascade_leaves_an_unknown_channel_alone() {
    let mut cascade = BiquadCascade::with_channels(2);
    let peak = BiquadBand {
      enabled: true,
      filter_type: FilterType::Peaking,
      frequency: 1000.0,
      gain_db: 12.0,
      q: 1.0,
      slope_db: 12,
    };
    cascade.set_bands(&[peak], 48_000.0);
    let mut buf = vec![0.5f32; 300];
    cascade.process_strided(&mut buf, 3, 2);
    assert!(buf.iter().all(|&s| s == 0.5));
  }

  #[test]
  fn bogus_rates_and_gains_give_finite_coefficients_without_panicking() {
    let types = [FilterType::Peaking, FilterType::LowShelf, FilterType::HighShelf, FilterType::LowPass];
    for rate in [0.0, f32::NAN, 22.0, -48_000.0, 48_000.0] {
      for gain_db in [f32::INFINITY, f32::NAN, f32::MAX, 800.0, -800.0, 6.0] {
        for (filter_type, frequency) in types.iter().copied().zip([f32::NAN, 1e9, 0.0, 1000.0]) {
          let band = BiquadBand { enabled: true, filter_type, frequency, gain_db, q: f32::NAN, slope_db: 12 };
          let c = BiquadCoefficients::from_band(&band, rate);
          assert!([c.b0, c.b1, c.b2, c.a1, c.a2].iter().all(|v| v.is_finite()), "{band:?} @ {rate}");
        }
      }
    }
  }

  #[test]
  fn test_peaking_filter_gain() {
    let band = BiquadBand {
      enabled: true,
      filter_type: FilterType::Peaking,
      frequency: 1000.0,
      gain_db: 6.0,
      q: 1.414,
      slope_db: 12,
    };
    let coef = BiquadCoefficients::from_band(&band, 48000.0);
    let resp_center = coef.response_db_at(1000.0, 48000.0);
    assert!((resp_center - 6.0).abs() < 0.1, "Expected ~6 dB at center, got {}", resp_center);

    let resp_far = coef.response_db_at(100.0, 48000.0);
    assert!(resp_far.abs() < 0.5, "Expected ~0 dB far from center, got {}", resp_far);
  }

  #[test]
  fn test_cascade_applies_gain_to_sine() {
    let band = BiquadBand { gain_db: 6.0, frequency: 1000.0, ..BiquadBand::default() };
    let mut cascade = BiquadCascade::default();
    cascade.set_bands(&[band], 48000.0);

    let mut buf: Vec<f32> = (0..48000).map(|i| (2.0 * PI * 1000.0 * i as f32 / 48000.0).sin() * 0.1).collect();
    cascade.process(&mut buf);
    let peak = buf.iter().skip(24000).fold(0.0f32, |m, s| m.max(s.abs()));
    let gain_db = 20.0 * (peak / 0.1).log10();
    assert!((gain_db - 6.0).abs() < 0.3, "expected ~+6 dB, got {gain_db}");
  }

  #[test]
  fn test_strided_processing_touches_only_its_channel() {
    let band = BiquadBand { gain_db: 12.0, frequency: 1000.0, ..BiquadBand::default() };
    let mut cascade = BiquadCascade::default();
    cascade.set_bands(&[band], 48000.0);
    let mut buf: Vec<f32> = (0..9600)
      .flat_map(|i| {
        let s = (2.0 * PI * 1000.0 * i as f32 / 48000.0).sin() * 0.1;
        [s, s]
      })
      .collect();
    cascade.process_strided(&mut buf, 2, 1);
    let tail = buf.get(9600..).unwrap_or_default();
    let peak_l = tail.iter().step_by(2).fold(0.0f32, |m, s| m.max(s.abs()));
    let peak_r = tail.iter().skip(1).step_by(2).fold(0.0f32, |m, s| m.max(s.abs()));
    assert!((peak_l - 0.1).abs() < 1e-3, "left untouched, got {peak_l}");
    assert!(peak_r > 0.35, "right boosted ~+12 dB, got {peak_r}");
  }

  #[test]
  fn test_cascade_skips_disabled_bands() {
    let band = BiquadBand { enabled: false, gain_db: 12.0, ..BiquadBand::default() };
    let mut cascade = BiquadCascade::default();
    cascade.set_bands(&[band], 48000.0);
    assert!(cascade.is_empty());
  }

  #[test]
  fn interleaved_matches_per_channel_strided_and_recovers_from_nan() {
    let bands = vec![
      BiquadBand { filter_type: FilterType::LowShelf, frequency: 120.0, gain_db: 4.5, q: 0.7, ..BiquadBand::default() },
      BiquadBand { frequency: 2500.0, gain_db: -3.0, q: 1.5, ..BiquadBand::default() },
      BiquadBand { filter_type: FilterType::HighPass, frequency: 80.0, slope_db: 24, ..BiquadBand::default() },
    ];
    for channels in [1usize, 2, 3, 4, 6, 8] {
      let mut interleaved = BiquadCascade::with_channels(channels);
      interleaved.set_bands(&bands, 48_000.0);
      let mut per_ch: Vec<BiquadCascade> = (0..channels).map(|_| BiquadCascade::default()).collect();
      per_ch.iter_mut().for_each(|c| c.set_bands(&bands, 48_000.0));

      let input: Vec<f32> = (0..960 * channels)
        .map(|i| ((i % channels + 1) as f32 * 0.05) * (2.0 * PI * 440.0 * (i / channels) as f32 / 48_000.0).sin())
        .collect();
      let mut a = input.clone();
      let mut b = input;
      interleaved.process_interleaved(&mut a, channels);
      for (ch, c) in per_ch.iter_mut().enumerate() {
        c.process_strided(&mut b, channels, ch);
      }
      assert_eq!(a, b, "interleaved and strided must match bit-for-bit ({channels} ch)");

      // NaN block resets state so the following finite block recovers cleanly.
      let mut bad = vec![f32::NAN; 16 * channels];
      interleaved.process_interleaved(&mut bad, channels);
      let mut good = vec![0.1f32; 64 * channels];
      interleaved.process_interleaved(&mut good, channels);
      assert!(good.iter().all(|s| s.is_finite()));
    }
  }
}
