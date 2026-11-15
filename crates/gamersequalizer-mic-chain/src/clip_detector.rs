//! Clipped raw input, two kinds: samples at digital full scale, and flat tops below it. A sound
//! card whose preamp saturates, or whose driver scales the signal after the converter clipped,
//! cuts the wave at a level under 0 dBFS: the top is then a run of (nearly) equal samples.
//! Natural speech never holds still that long at that level: even an 80 Hz voice at -6 dBFS moves
//! more than one 16-bit step within 3 samples of its peak. Cost: one compare per sample.
//!
//! Known limit: a soft (rounded) analog clip, or one smoothed by a resampler, is not flat and is
//! not counted; only digital-looking clipping is. Upgrade path: compare the crest factor of
//! speech frames against the learned voice, if users report distortion that this misses.

/// A sample this close to full scale counts as clipped.
const FULL_SCALE: f32 = 0.98;
/// Flat tops count only above this level (-12 dBFS): quiet steady signals (DC, silence) are no clip.
const FLAT_MIN_LEVEL: f32 = 0.25;
/// Neighbours this close count as equal (one 16-bit step).
const FLAT_EPS: f32 = 1.0 / 32768.0;
/// Equal samples in a row that make a flat top (0.125 ms at 48 kHz).
const FLAT_RUN: u32 = 6;

#[derive(Debug, Default)]
pub struct ClipDetector {
  prev: f32,
  /// Equal neighbour pairs in a row so far (a run of n samples has n - 1 pairs).
  pairs: u32,
}

impl ClipDetector {
  /// Clipped samples in `samples` (mono, raw input). Keeps state across blocks.
  pub fn count(&mut self, samples: &[f32]) -> u32 {
    let mut clipped = 0;
    for &s in samples {
      let a = s.abs();
      let flat = (FLAT_MIN_LEVEL..FULL_SCALE).contains(&a) && (s - self.prev).abs() <= FLAT_EPS;
      self.pairs = if flat { self.pairs + 1 } else { 0 };
      if a >= FULL_SCALE {
        clipped += 1;
      } else if self.pairs + 1 == FLAT_RUN {
        clipped += FLAT_RUN; // the whole run so far
      } else if self.pairs + 1 > FLAT_RUN {
        clipped += 1;
      }
      self.prev = s;
    }
    clipped
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const SR: f32 = 48_000.0;

  fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
    (0..n).map(|i| amp * (i as f32 * freq * std::f32::consts::TAU / SR).sin()).collect()
  }

  /// 16-bit quantization, as most mic converters deliver.
  fn q16(x: &[f32]) -> Vec<f32> {
    x.iter().map(|s| (s * 32768.0).round() / 32768.0).collect()
  }

  fn count_in_blocks(x: &[f32]) -> u32 {
    let mut d = ClipDetector::default();
    x.chunks(441).map(|b| d.count(b)).sum()
  }

  #[test]
  fn clean_low_voice_near_full_scale_is_not_clipped() {
    for freq in [80.0, 120.0, 250.0, 1000.0] {
      let x = q16(&sine(freq, 0.95, 48_000));
      assert_eq!(count_in_blocks(&x), 0, "{freq} Hz");
    }
  }

  #[test]
  fn quiet_steady_signal_is_not_clipped() {
    assert_eq!(count_in_blocks(&vec![0.1; 4800]), 0);
    assert_eq!(count_in_blocks(&vec![0.0; 4800]), 0);
  }

  #[test]
  fn full_scale_samples_count_once() {
    let x = q16(&sine(200.0, 1.5, 4800)).iter().map(|s| s.clamp(-1.0, 1.0)).collect::<Vec<_>>();
    let expected = x.iter().filter(|s| s.abs() >= FULL_SCALE).count() as u32;
    assert!(expected > 0);
    assert_eq!(count_in_blocks(&x), expected);
  }

  #[test]
  fn flat_top_below_full_scale_is_clipped() {
    // Converter clipped at 0 dBFS, then the driver scaled by -6 dB: the rail sits at 0.5.
    let x: Vec<f32> = q16(&sine(200.0, 2.0, 4800)).iter().map(|s| s.clamp(-1.0, 1.0) * 0.5).collect();
    let rail = x.iter().filter(|s| s.abs() >= 0.5 - FLAT_EPS).count() as u32;
    let found = count_in_blocks(&x);
    assert!(found > rail * 9 / 10 && found <= rail, "{found} of {rail}");
  }
}
