use super::biquad::BiquadCoefficients;
use gamersequalizer_common::profile::{expand_slopes, BiquadBand};

pub const FREQ_POINTS_COUNT: usize = 512;
pub const MIN_FREQ: f32 = 20.0;
pub const MAX_FREQ: f32 = 20000.0;
pub const DEFAULT_SAMPLE_RATE: f32 = 48000.0;

#[derive(Debug, Clone)]
pub struct CurveResponse {
  pub frequencies: Vec<f32>,
  pub gains_db: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct CombinedEngineResult {
  pub layer_a_curve: CurveResponse,
  pub layer_b_curve: CurveResponse,
  pub combined_curve: CurveResponse,
  pub auto_preamp_db: f32,
  pub max_boost_db: f32,
}

/// Calculate 512 log-spaced frequencies from 20 Hz to 20,000 Hz
pub fn generate_log_frequencies() -> Vec<f32> {
  let log_min = MIN_FREQ.ln();
  let log_max = MAX_FREQ.ln();
  let step = (log_max - log_min) / (FREQ_POINTS_COUNT - 1) as f32;

  (0..FREQ_POINTS_COUNT).map(|i| (log_min + i as f32 * step).exp()).collect()
}

/// Evaluate frequency response curve for a slice of bands
pub fn evaluate_bands_response(bands: &[BiquadBand], frequencies: &[f32], sample_rate: f32) -> Vec<f32> {
  let active_coefs: Vec<BiquadCoefficients> =
    expand_slopes(bands).iter().filter(|b| b.enabled).map(|b| BiquadCoefficients::from_band(b, sample_rate)).collect();

  frequencies.iter().map(|&f| active_coefs.iter().map(|c| c.response_db_at(f, sample_rate)).sum::<f32>()).collect()
}

/// Combine Layer A (Headphone calibration) + Layer B (Game Profile) and compute anti-clipping preamp
pub fn compute_combined_engine(
  layer_a_bands: &[BiquadBand],
  layer_b_bands: &[BiquadBand],
  sample_rate: f32,
) -> CombinedEngineResult {
  let frequencies = generate_log_frequencies();

  let layer_a_gains = evaluate_bands_response(layer_a_bands, &frequencies, sample_rate);
  let layer_b_gains = evaluate_bands_response(layer_b_bands, &frequencies, sample_rate);

  let mut combined_gains = Vec::with_capacity(FREQ_POINTS_COUNT);
  let mut max_boost = 0.0f32;

  for (a, b) in layer_a_gains.iter().zip(&layer_b_gains) {
    let combined = a + b;
    if combined > max_boost {
      max_boost = combined;
    }
    combined_gains.push(combined);
  }

  // Preamp: -(G_max + 0.5) dB to strictly prevent clipping
  let auto_preamp_db = if max_boost > 0.0 { -(max_boost + 0.5) } else { 0.0 };

  CombinedEngineResult {
    layer_a_curve: CurveResponse { frequencies: frequencies.clone(), gains_db: layer_a_gains },
    layer_b_curve: CurveResponse { frequencies: frequencies.clone(), gains_db: layer_b_gains },
    combined_curve: CurveResponse { frequencies, gains_db: combined_gains },
    auto_preamp_db,
    max_boost_db: max_boost,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use gamersequalizer_common::profile::FilterType;

  #[test]
  fn test_auto_preamp_calculation() {
    let band_a = BiquadBand {
      enabled: true,
      filter_type: FilterType::Peaking,
      frequency: 1000.0,
      gain_db: 4.0,
      q: 1.414,
      slope_db: 12,
    };
    let band_b = BiquadBand {
      enabled: true,
      filter_type: FilterType::Peaking,
      frequency: 1000.0,
      gain_db: 3.0,
      q: 1.414,
      slope_db: 12,
    };

    let result = compute_combined_engine(&[band_a], &[band_b], 48000.0);
    assert!((result.max_boost_db - 7.0).abs() < 0.2);
    assert!((result.auto_preamp_db - (-7.5)).abs() < 0.2);
  }
}
