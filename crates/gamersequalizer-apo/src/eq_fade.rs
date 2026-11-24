//! An EQ that fades in and out instead of jumping, for the output and the stream processors.

use super::*;

/// Preamp and cascade, plus the share of the processed signal in the output (`wet`, 0..1) that
/// moves by `step` per frame toward its goal. Losing the owner (service hung, resume from
/// sleep), a bypass toggle or a channel change fades over `STREAM_FADE_SECONDS` instead of
/// jumping (a -6 dB preamp switching to 0 dB at once, the limiter dropping out mid-peak).
pub(super) struct EqFade {
  pub(super) cascade: BiquadCascade,
  /// The gain in use; moves to `preamp_target` over one block (`run`).
  preamp_gain: f32,
  preamp_target: f32,
  pub(super) wet: f32,
  step: f32,
  /// Processed copy of the block while fading. Grows (allocates) only if the engine sends a
  /// larger buffer than before.
  scratch: Vec<f32>,
}

impl EqFade {
  pub(super) fn new(cascade: BiquadCascade, sample_rate: f32, scratch_capacity: usize) -> Self {
    EqFade {
      cascade,
      preamp_gain: 1.0,
      preamp_target: 1.0,
      wet: 0.0,
      step: 1.0 / (STREAM_FADE_SECONDS * sample_rate.max(1.0)),
      scratch: Vec::with_capacity(scratch_capacity),
    }
  }

  pub(super) fn set_preamp_db(&mut self, db: f32) {
    self.preamp_target = 10f32.powf(db / 20.0);
    // While faded out (initial setup / bypass), snap so the fade-in starts at the target gain.
    if self.wet == 0.0 {
      self.preamp_gain = self.preamp_target;
    }
  }

  /// Preamp (ramped smoothly across a block on slider drags), then each channel through its own
  /// cascade, in place.
  pub(super) fn run(&mut self, buf: &mut [f32], channels: usize) {
    if (self.preamp_gain - self.preamp_target).abs() > 1e-6 && channels > 0 {
      let frames = buf.len() / channels;
      if frames > 0 {
        let step = (self.preamp_target - self.preamp_gain) / frames as f32;
        for frame in buf.chunks_exact_mut(channels) {
          self.preamp_gain += step;
          for s in frame {
            *s *= self.preamp_gain;
          }
        }
      }
      self.preamp_gain = self.preamp_target;
    } else {
      self.preamp_gain = self.preamp_target;
      if self.preamp_gain != 1.0 {
        buf.iter_mut().for_each(|s| *s *= self.preamp_gain);
      }
    }
    self.cascade.process_interleaved(buf, channels);
  }

  /// Dry + wet * (processed - dry), with `wet` ramping toward `goal` (0 or 1) per frame.
  /// `after` runs on the processed copy after the EQ (the output's limiter).
  pub(super) fn crossfade(&mut self, buf: &mut [f32], channels: usize, goal: f32, after: impl FnOnce(&mut [f32])) {
    if self.scratch.len() < buf.len() {
      self.scratch.resize(buf.len(), 0.0);
    }
    let mut scratch = std::mem::take(&mut self.scratch);
    if let Some(wet_buf) = scratch.get_mut(..buf.len()) {
      wet_buf.copy_from_slice(buf);
      self.run(wet_buf, channels);
      after(wet_buf);
      for (dry_frame, wet_frame) in buf.chunks_exact_mut(channels).zip(wet_buf.chunks_exact(channels)) {
        self.wet = if goal > self.wet { (self.wet + self.step).min(1.0) } else { (self.wet - self.step).max(0.0) };
        for (d, w) in dry_frame.iter_mut().zip(wet_frame) {
          *d += self.wet * (w - *d);
        }
      }
    }
    self.scratch = scratch;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_preamp_change_ramps_over_one_block_only_while_the_eq_is_in() {
    let mut eq = EqFade::new(BiquadCascade::with_channels(2), 48_000.0, 0);
    let run = |eq: &mut EqFade| {
      let mut buf = [1.0f32; 8];
      eq.run(&mut buf, 2);
      buf
    };
    // Faded out: the fade-in starts at the new gain.
    eq.set_preamp_db(-6.0);
    assert!(run(&mut eq).iter().all(|s| (s - 0.501).abs() < 0.001));

    eq.wet = 1.0;
    eq.set_preamp_db(0.0);
    let ramp = run(&mut eq);
    assert!(ramp.as_chunks::<2>().0.iter().all(|[l, r]| l == r), "{ramp:?}");
    assert!(ramp.is_sorted() && ramp[0] > 0.55 && ramp[0] < 0.7 && (ramp[7] - 1.0).abs() < 1e-5, "{ramp:?}");
    assert_eq!(run(&mut eq), [1.0; 8], "the next block is at the new gain");
  }
}
