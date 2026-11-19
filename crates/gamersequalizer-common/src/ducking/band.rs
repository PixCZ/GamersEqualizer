//! The speech-band filter both duckers measure with, and the stream ducker cuts with.

use super::DuckTuning;
use std::f64::consts::PI;

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// Filter state under this is zeroed: with exact-zero input it would otherwise decay into the
/// subnormal range, which costs CPU per sample unless the thread flushes denormals.
const TINY_STATE: f64 = 1e-30;

// ---------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------

/// RBJ band-pass with 0 dB peak gain, normalized (b1 = 0, b2 = -b0). With it, `x - c * bp(x)` is
/// exactly a bell of gain `1 - c` at the centre, so the depth moves without new coefficients.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct BandPass {
  b0: f64,
  a1: f64,
  a2: f64,
}

// ---------------------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------------------

impl BandPass {
  pub(super) fn new(sample_rate: f64, t: &DuckTuning) -> Self {
    // Below Nyquist whatever the tuning says.
    let w0 = 2.0 * PI * f64::from(t.band_center_hz).min(0.45 * sample_rate) / sample_rate;
    let alpha = w0.sin() / (2.0 * f64::from(t.band_q));
    let a0 = 1.0 + alpha;
    BandPass { b0: alpha / a0, a1: -2.0 * w0.cos() / a0, a2: (1.0 - alpha) / a0 }
  }

  /// One sample through the transposed-direct-form-II state `z`.
  pub(super) fn run(&self, z: &mut [f64; 2], x: f64) -> f64 {
    let [z1, z2] = *z;
    let y = self.b0 * x + z1;
    *z = [z2 - self.a1 * y, -self.b0 * x - self.a2 * y];
    y
  }

  /// Mean square of the band over interleaved `buf` (one state per channel), advancing the state;
  /// 0 when not finite.
  pub(super) fn measure(&self, state: &mut [[f64; 2]], buf: &[f32], channels: usize) -> f32 {
    let mut sum = 0.0f64;
    let mut count = 0usize;
    for frame in buf.chunks_exact(channels.max(1)) {
      for (x, z) in frame.iter().zip(state.iter_mut()) {
        let y = self.run(z, f64::from(*x));
        sum += y * y;
      }
      count += channels;
    }
    flush(state);
    let ms = (sum / count.max(1) as f64) as f32;
    if ms.is_finite() { ms } else { 0.0 }
  }
}

/// Zeroes non-finite state (one NaN/inf input sample must not poison the filter for the life of
/// the stream) and tiny state (`TINY_STATE`). Once per block is enough: from 1e-30 the state needs
/// far longer than a block to become subnormal.
pub(super) fn flush(state: &mut [[f64; 2]]) {
  for z in state {
    if !z.iter().all(|v| v.is_finite()) {
      *z = [0.0; 2];
    }
    for v in z.iter_mut().filter(|v| v.abs() < TINY_STATE) {
      *v = 0.0;
    }
  }
}
