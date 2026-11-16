//! The heavy model's sleep and the handover between it and the RNNoise stand-in.

/// The heavy model's sleep: what a pause, a model switch and a reset all start over (`clear`).
/// The room's noise floor (`note_activity`) is not part of it: a pause doesn't change the room.
pub(super) struct Sleep {
  /// Heavy model asleep: no speech for `sleep_after` samples, so RNNoise (delayed to the heavy
  /// model's latency) covers the pause and the heavy model stops (DFN3 ~3 %, DPDFNet ~10 % of a
  /// core). Speech on RNNoise's own detector wakes it.
  pub(super) asleep: bool,
  /// Both run while one takes over from the other (see `Takeover`).
  pub(super) takeover: Option<Takeover>,
  /// Input samples since the input detector last heard speech or saw activity.
  pub(super) quiet_samples: usize,
  /// RNNoise output delayed to the heavy model's latency while it stands in.
  pub(super) delay: DelayLine,
}

/// The heavy model and the RNNoise stand-in hand over without a gap: from the start both get
/// the input; the output stays on the old one until the newcomer's first real sample comes out
/// (both paths have the heavy model's latency, so they are aligned), then crossfades. The input
/// detector keeps its own pass until the heavy model is off.
#[derive(Clone, Copy)]
pub(super) struct Takeover {
  pub(super) to_sleep: bool,
  /// Output samples since the newcomer started.
  pub(super) done: usize,
  /// Output sample the crossfade starts at: the newcomer's latency (and RNNoise's warm-up).
  pub(super) fade_at: usize,
}

/// Fixed delay (ring buffer); empty = no delay. Allocates only when its length changes.
pub(super) struct DelayLine {
  buf: Vec<f32>,
  pos: usize,
}

impl Sleep {
  pub(super) fn clear(&mut self) {
    (self.asleep, self.takeover, self.quiet_samples) = (false, None, 0);
    self.delay.clear();
  }
}

impl DelayLine {
  pub(super) fn new() -> Self {
    Self { buf: Vec::new(), pos: 0 }
  }

  pub(super) fn len(&self) -> usize {
    self.buf.len()
  }

  /// Changing the length restarts from silence: a gap once, when the setting changes.
  pub(super) fn set_len(&mut self, len: usize) {
    if len != self.buf.len() {
      self.buf = vec![0.0; len];
      self.pos = 0;
    }
  }

  pub(super) fn clear(&mut self) {
    self.buf.fill(0.0);
    self.pos = 0;
  }

  pub(super) fn process(&mut self, samples: &mut [f32]) {
    let len = self.buf.len();
    if len == 0 {
      return;
    }
    for s in samples.iter_mut() {
      if let Some(slot) = self.buf.get_mut(self.pos) {
        std::mem::swap(slot, s);
      }
      self.pos = (self.pos + 1) % len;
    }
  }
}
