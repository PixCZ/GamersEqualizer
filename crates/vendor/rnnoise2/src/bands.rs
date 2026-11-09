//! Bands and features: `eband20ms` band energies and correlations, gain interpolation, the
//! 32-point DCT and the analysis window (`denoise.c`).

// Every index in this module is a loop bound or a value of `EBAND20MS` on fixed-size arrays of
// `FREQ_SIZE` / `NB_BANDS (+2)` entries, all compile-time constants (max band edge 400 < 481).
#![allow(clippy::indexing_slicing)]

use rustfft::num_complex::Complex32;

pub(crate) const FRAME_SIZE: usize = 480;
pub(crate) const WINDOW_SIZE: usize = 2 * FRAME_SIZE;
pub(crate) const FREQ_SIZE: usize = FRAME_SIZE + 1;
pub(crate) const NB_BANDS: usize = 32;
/// `NB_FEATURES`: 32 band cepstra, 32 pitch-correlation DCT terms, pitch period.
pub(crate) const NB_FEATURES: usize = 2 * NB_BANDS + 1;

/// Band edges in bins of 100 Hz (`eband20ms`); bins >= 400 (20 kHz) belong to no band.
const EBAND20MS: [usize; NB_BANDS + 2] = [
  0, 2, 4, 6, 8, 10, 12, 15, 18, 21, 24, 28, 32, 36, 41, 47, 53, 60, 68, 77, 87, 98, 110, 124, 140, 157, 176, 198, 223,
  251, 282, 317, 356, 400,
];

/// The analysis/synthesis window and the DCT table, computed like `dump_rnnoise_tables.c`.
#[derive(Clone)]
pub(crate) struct Tables {
  /// `rnn_half_window`: first half of the symmetric Vorbis power-complementary window.
  pub(crate) half_window: [f32; FRAME_SIZE],
  /// `rnn_dct_table[i*32 + j] = cos((i + .5) j pi / 32)`, column 0 times `sqrt(.5)`.
  dct_table: [f32; NB_BANDS * NB_BANDS],
}

impl Tables {
  pub(crate) fn new() -> Self {
    use std::f64::consts::PI;
    let mut half_window = [0.0f32; FRAME_SIZE];
    for (i, w) in half_window.iter_mut().enumerate() {
      let s = (0.5 * PI * (i as f64 + 0.5) / FRAME_SIZE as f64).sin();
      *w = (0.5 * PI * s * s).sin() as f32;
    }
    let mut dct_table = [0.0f32; NB_BANDS * NB_BANDS];
    for i in 0..NB_BANDS {
      for j in 0..NB_BANDS {
        let mut v = ((i as f64 + 0.5) * j as f64 * PI / NB_BANDS as f64).cos() as f32;
        if j == 0 {
          v = (v as f64 * 0.5f64.sqrt()) as f32;
        }
        dct_table[i * NB_BANDS + j] = v;
      }
    }
    Tables { half_window, dct_table }
  }

  /// `apply_window` on a full `WINDOW_SIZE` buffer (both halves, mirrored).
  pub(crate) fn apply_window(&self, x: &mut [f32; WINDOW_SIZE]) {
    for (i, &w) in self.half_window.iter().enumerate() {
      x[i] *= w;
      x[WINDOW_SIZE - 1 - i] *= w;
    }
  }

  /// `dct`: `out[i] = (f32)(sum_j in[j] * table[j*32 + i] * sqrt(2/22))`. The `2/22` is upstream's
  /// (left over from 22 bands); the network was trained with it.
  pub(crate) fn dct(&self, out: &mut [f32], input: &[f32; NB_BANDS]) {
    for (i, o) in out.iter_mut().take(NB_BANDS).enumerate() {
      let mut sum = 0.0f32;
      for (j, &x) in input.iter().enumerate() {
        sum += x * self.dct_table[j * NB_BANDS + i];
      }
      *o = (sum as f64 * (2.0f64 / 22.0).sqrt()) as f32;
    }
  }
}

/// `compute_band_energy`: triangular bands over `|X|^2`.
pub(crate) fn band_energy(x: &[Complex32; FREQ_SIZE]) -> [f32; NB_BANDS] {
  band_sum(|i| {
    let mut tmp = x[i].re * x[i].re;
    tmp += x[i].im * x[i].im;
    tmp
  })
}

/// `compute_band_corr`: triangular bands over `Re(X conj(P))`.
pub(crate) fn band_corr(x: &[Complex32; FREQ_SIZE], p: &[Complex32; FREQ_SIZE]) -> [f32; NB_BANDS] {
  band_sum(|i| {
    let mut tmp = x[i].re * p[i].re;
    tmp += x[i].im * p[i].im;
    tmp
  })
}

/// Spreads each bin over its two neighbouring band edges (34 sums), then folds the two outer
/// half bands into their neighbours: `sum[1] = (sum[0] + sum[1]) * 2/3`, same at the top.
fn band_sum(value: impl Fn(usize) -> f32) -> [f32; NB_BANDS] {
  let mut sum = [0.0f32; NB_BANDS + 2];
  for i in 0..NB_BANDS + 1 {
    let band_size = EBAND20MS[i + 1] - EBAND20MS[i];
    for j in 0..band_size {
      let frac = j as f32 / band_size as f32;
      let tmp = value(EBAND20MS[i] + j);
      sum[i] += (1.0 - frac) * tmp;
      sum[i + 1] += frac * tmp;
    }
  }
  sum[1] = (sum[0] + sum[1]) * 2.0 / 3.0;
  sum[NB_BANDS] = (sum[NB_BANDS] + sum[NB_BANDS + 1]) * 2.0 / 3.0;
  let mut band = [0.0f32; NB_BANDS];
  band.copy_from_slice(&sum[1..NB_BANDS + 1]);
  band
}

/// `interp_band_gain`: per-bin gain from band gains. Bins >= 400 are set to 0, which is what
/// upstream's zero-initialised callers end up with (it never writes them).
pub(crate) fn interp_band_gain(g: &mut [f32; FREQ_SIZE], band: &[f32; NB_BANDS]) {
  g.fill(0.0);
  for i in 1..NB_BANDS {
    let band_size = EBAND20MS[i + 1] - EBAND20MS[i];
    for j in 0..band_size {
      let frac = j as f32 / band_size as f32;
      g[EBAND20MS[i] + j] = (1.0 - frac) * band[i - 1] + frac * band[i];
    }
  }
  for v in &mut g[..EBAND20MS[1]] {
    *v = band[0];
  }
  for v in &mut g[EBAND20MS[NB_BANDS]..EBAND20MS[NB_BANDS + 1]] {
    *v = band[NB_BANDS - 1];
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn white_noise_band_energies_sum_to_spectrum_energy() {
    // Deterministic pseudo-random spectrum (xorshift).
    let mut seed = 0x1234_5678u32;
    let mut rand = || {
      seed ^= seed << 13;
      seed ^= seed >> 17;
      seed ^= seed << 5;
      seed as f32 / u32::MAX as f32 - 0.5
    };
    let mut x = [Complex32::new(0.0, 0.0); FREQ_SIZE];
    x.iter_mut().for_each(|c| *c = Complex32::new(rand(), rand()));
    let bands: f32 = band_energy(&x).iter().sum();
    // Every bin below 400 is split between two edges with weights summing to 1, except that the
    // folding scales the outer half bands (bins 0..2 and 356..400, partly) by 2/3. Mirror that:
    let mut sum = [0.0f32; NB_BANDS + 2];
    for i in 0..NB_BANDS + 1 {
      let size = EBAND20MS[i + 1] - EBAND20MS[i];
      for j in 0..size {
        let e = x[EBAND20MS[i] + j].norm_sqr();
        sum[i] += (1.0 - j as f32 / size as f32) * e;
        sum[i + 1] += j as f32 / size as f32 * e;
      }
    }
    let expected = sum.iter().sum::<f32>() - (sum[0] + sum[1] + sum[NB_BANDS] + sum[NB_BANDS + 1]) / 3.0;
    assert!((bands - expected).abs() < 0.01 * expected, "{bands} vs {expected}");
  }

  #[test]
  fn interp_band_gain_is_flat_for_flat_gains_and_zero_above_400() {
    let mut g = [7.0f32; FREQ_SIZE];
    interp_band_gain(&mut g, &[0.5; NB_BANDS]);
    assert!(g[..400].iter().all(|&v| (v - 0.5).abs() < 1e-6));
    assert!(g[400..].iter().all(|&v| v == 0.0));
  }

  #[test]
  #[allow(clippy::excessive_precision, reason = "literals copied verbatim from rnnoise_tables.c")]
  fn dct_table_matches_upstream_literals() {
    let t = Tables::new();
    // First values of `rnn_dct_table` and `rnn_half_window` in `rnnoise_tables.c`.
    assert_eq!(&t.dct_table[..5], &[0.707106769f32, 0.998795450, 0.995184720, 0.989176512, 0.980785251]);
    assert_eq!(
      &t.half_window[..5],
      &[4.20549168e-06f32, 3.78491532e-05, 0.000105135041, 0.000206060256, 0.000340620492]
    );
    assert_eq!(&t.dct_table[NB_BANDS * NB_BANDS - 4..], &[0.195090324f32, -0.146730468, 0.0980171412, -0.0490676761]);
  }
}
