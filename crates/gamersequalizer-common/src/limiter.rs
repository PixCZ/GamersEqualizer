//! Limiters. `soft_limit` (mic chain): a static curve, transparent below the knee and
//! saturating within ~0.05 above it, so it only rounds off the last fraction of a dB.
//! `PeakLimiter` (output EQ): a gain envelope that turns peaks of several dB down smoothly
//! (instant attack, ~80 ms release), with `soft_limit` as the final safety net.

/// -0.45 dBFS: everything quieter passes bit-exact.
pub const LIMITER_KNEE: f32 = 0.95;

pub fn soft_limit(x: f32) -> f32 {
  // The last stage before the output: never let NaN (`signum` keeps it) or inf
  // (`inf / inf`) through.
  if !x.is_finite() {
    return 0.0;
  }
  let abs = x.abs();
  if abs <= LIMITER_KNEE {
    x
  } else {
    x.signum() * (LIMITER_KNEE + (abs - LIMITER_KNEE) / (1.0 + abs - LIMITER_KNEE)).min(1.0)
  }
}

/// Peaks are brought down to this level (-0.26 dBFS).
const PEAK_CEILING: f32 = 0.97;
const RELEASE_SECONDS: f32 = 0.08;

/// Shared gain for all channels of an interleaved stream (keeps the stereo image).
#[derive(Debug, Clone)]
pub struct PeakLimiter {
  gain: f32,
  release_coef: f32,
}

impl PeakLimiter {
  pub fn new(sample_rate: f32) -> Self {
    let samples = (RELEASE_SECONDS * sample_rate.max(1.0)).max(1.0);
    Self { gain: 1.0, release_coef: 1.0 - (-1.0 / samples).exp() }
  }

  /// In place on interleaved `buf`. Below the ceiling with gain recovered, it changes nothing.
  pub fn process(&mut self, buf: &mut [f32], channels: usize) {
    for frame in buf.chunks_mut(channels.max(1)) {
      let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
      let needed = if peak * self.gain > PEAK_CEILING { PEAK_CEILING / peak } else { 1.0 };
      self.gain = if needed < self.gain { needed } else { self.gain + (1.0 - self.gain) * self.release_coef };
      if !self.gain.is_finite() {
        self.gain = 1.0;
      }
      for s in frame.iter_mut() {
        *s = soft_limit(*s * self.gain);
      }
    }
  }

  pub fn gain(&self) -> f32 {
    self.gain
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn soft_limit_never_passes_nan_or_inf() {
    for x in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
      assert_eq!(soft_limit(x), 0.0, "{x}");
    }
    assert_eq!(soft_limit(0.5), 0.5);
    assert!(soft_limit(3.0) <= 1.0);
  }

  #[test]
  fn peak_limiter_keeps_peaks_under_full_scale_and_recovers() {
    let mut lim = PeakLimiter::new(48_000.0);
    // +6 dB sine burst (amplitude 2.0) on both channels.
    let mut loud: Vec<f32> = (0..4800).flat_map(|i| [(i as f32 * 0.13).sin() * 2.0; 2]).collect();
    lim.process(&mut loud, 2);
    assert!(loud.iter().all(|s| s.abs() <= 1.0));
    assert!(lim.gain() < 0.6);
    // Quiet audio afterwards: gain returns to unity within ~0.5 s and passes unchanged.
    let mut quiet = vec![0.1f32; 48_000];
    lim.process(&mut quiet, 2);
    assert!(lim.gain() > 0.999);
    let mut tail = vec![0.3f32, -0.3];
    lim.process(&mut tail, 2);
    assert!((tail[0] - 0.3).abs() < 1e-3);
  }

  #[test]
  fn transparent_below_knee_and_bounded_above() {
    assert_eq!(soft_limit(0.5), 0.5);
    assert_eq!(soft_limit(-0.9), -0.9);
    assert!(soft_limit(2.0) <= 1.0 && soft_limit(2.0) > LIMITER_KNEE);
    assert!(soft_limit(-8.0) >= -1.0);
    assert!(soft_limit(0.97) < soft_limit(0.99), "rising above the knee");
    assert!(soft_limit(1.2) <= soft_limit(1.5), "never decreasing");
  }
}
