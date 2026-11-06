use crate::profile::{expand_slopes, BiquadBand, FilterType};
use std::f32::consts::PI;

/// Upper bound of microphone EQ bands (also its shared-memory slot count).
pub const MAX_EQ_BANDS: usize = 16;
/// Output EQ: headphone correction (AutoEQ uses up to 10) + game profile (Sonar up to 10).
pub const MAX_OUT_EQ_BANDS: usize = 32;
/// Biquads of one stream EQ (chat/media profile + the inverted game profile); also the
/// most stages a cascade runs.
pub const MAX_STREAM_EQ_BANDS: usize = 64;

#[derive(Debug, Clone, Copy)]
pub struct BiquadCoefficients {
  pub b0: f32,
  pub b1: f32,
  pub b2: f32,
  pub a0: f32,
  pub a1: f32,
  pub a2: f32,
}

/// Cascade of biquads processed in Transposed Direct Form II (f64 state: stable at low
/// frequencies such as an 80 Hz high-pass at 48 kHz).
#[derive(Debug, Clone, Default)]
pub struct BiquadCascade {
  stages: Vec<Stage>,
}

#[derive(Debug, Clone, Copy)]
struct Stage {
  b0: f64,
  b1: f64,
  b2: f64,
  a1: f64,
  a2: f64,
  z1: f64,
  z2: f64,
}

impl BiquadCoefficients {
  /// Calculate biquad coefficients using RBJ Audio-EQ-Cookbook formulas
  pub fn from_band(band: &BiquadBand, sample_rate: f32) -> Self {
    let f0 = band.frequency.clamp(20.0, sample_rate * 0.499);
    let q = band.q.clamp(0.1, 30.0);
    let gain_db = band.gain_db;

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

    Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a0: 1.0, a1: a1 / a0, a2: a2 / a0 }
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
  /// Rebuild coefficients for the enabled bands. Filter state is kept when the stage count
  /// is unchanged, so dragging a slider doesn't click.
  pub fn set_bands(&mut self, bands: &[BiquadBand], sample_rate: f32) {
    let new: Vec<BiquadCoefficients> = expand_slopes(bands)
      .iter()
      .filter(|b| b.enabled)
      .take(MAX_STREAM_EQ_BANDS)
      .map(|b| BiquadCoefficients::from_band(b, sample_rate))
      .collect();

    let keep_state = new.len() == self.stages.len();
    let old = std::mem::take(&mut self.stages);
    self.stages = new
      .iter()
      .enumerate()
      .map(|(i, c)| {
        let (z1, z2) = match old.get(i) {
          Some(s) if keep_state => (s.z1, s.z2),
          _ => (0.0, 0.0),
        };
        Stage { b0: c.b0 as f64, b1: c.b1 as f64, b2: c.b2 as f64, a1: c.a1 as f64, a2: c.a2 as f64, z1, z2 }
      })
      .collect();
  }

  pub fn is_empty(&self) -> bool {
    self.stages.is_empty()
  }

  pub fn process(&mut self, samples: &mut [f32]) {
    self.process_strided(samples, 1, 0);
  }

  /// Process one channel of an interleaved buffer (`channel` of `channels`). Each channel
  /// needs its own cascade (own filter state).
  pub fn process_strided(&mut self, samples: &mut [f32], channels: usize, channel: usize) {
    let step = channels.max(1);
    for stage in self.stages.iter_mut() {
      for s in samples.iter_mut().skip(channel).step_by(step) {
        let x = *s as f64;
        let y = stage.b0 * x + stage.z1;
        stage.z1 = stage.b1 * x - stage.a1 * y + stage.z2;
        stage.z2 = stage.b2 * x - stage.a2 * y;
        *s = y as f32;
      }
    }
    // Flush denormals / recover from NaN so one bad block can't poison the state forever.
    for stage in self.stages.iter_mut() {
      if !stage.z1.is_finite() || !stage.z2.is_finite() {
        stage.z1 = 0.0;
        stage.z2 = 0.0;
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

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
}
