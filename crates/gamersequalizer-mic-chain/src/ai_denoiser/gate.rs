//! The automatic speech gate.

use super::*;

/// Per-sample smoothing of the gate gain: ~1 ms to open, ~50 ms to close (no clicks).
const GATE_OPEN_COEF: f32 = 1.0 / 48.0;
const GATE_CLOSE_COEF: f32 = 1.0 / (48.0 * 50.0);

/// Automatic speech gate: open while a detector hears speech, plus a hold; fades instead of
/// chopping, and closed only attenuates (`GATE_FLOOR_DB`). No level threshold, so it works the
/// same for a loud or a quiet voice (day vs. evening). Classic attack/hold/release keyed by a VAD.
/// Tuned to never cut a voice; some loud keys get through (see `OUT_VOICE_PROB`).
pub(super) struct NoiseGate {
  pub(super) enabled: bool,
  pub(super) hold_left: usize,
  pub(super) gain: f32,
  pub(super) floor: f32,
}

impl NoiseGate {
  pub(super) fn new(enabled: bool) -> Self {
    Self { enabled, hold_left: 0, gain: 0.0, floor: 10f32.powf(GATE_FLOOR_DB / 20.0) }
  }

  /// `hold`: samples to stay open from now (speech heard in this block), 0 = none heard.
  /// Never shortens a running hold.
  pub(super) fn process(&mut self, samples: &mut [f32], hold: usize) {
    if !self.enabled {
      return;
    }
    self.hold_left = self.hold_left.max(hold);
    for s in samples.iter_mut() {
      self.hold_left = self.hold_left.saturating_sub(1);
      let (target, coef) = if self.hold_left > 0 { (1.0, GATE_OPEN_COEF) } else { (self.floor, GATE_CLOSE_COEF) };
      self.gain += (target - self.gain) * coef;
      *s *= self.gain;
    }
  }
}
