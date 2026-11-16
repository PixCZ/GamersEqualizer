//! RNNoise as the denoiser: frames of 480 samples, blended with the dry signal by strength.

use super::*;

/// Silent frames that push old audio out of RNNoise on a flush: its pitch buffer (1728 samples)
/// needs 4, its input high-pass (pole 0.998, ~500-sample time constant) 10 to fade under -80 dB.
/// About 0.2 ms of CPU, once per pause.
const RNNOISE_FLUSH_FRAMES: usize = 10;

pub(super) struct RnNoiseEngine {
  denoiser: Box<DenoiseState<'static>>,
  input: FrameInput,
  output: FrameOutput,
  /// Previous dry frame: RNNoise output lags its input by one frame, so the dry
  /// signal for the strength blend must be delayed by the same amount (else comb filtering).
  prev_dry: [f32; FRAME_SIZE],
}

impl RnNoiseEngine {
  pub(super) fn new() -> Self {
    Self {
      denoiser: DenoiseState::new(),
      input: FrameInput::new(FRAME_SIZE, FRAME_SIZE),
      output: FrameOutput::new(FRAME_SIZE),
      prev_dry: [0.0; FRAME_SIZE],
    }
  }

  /// `on_prob`: RNNoise's voice probability of each input frame it completes.
  pub(super) fn process(&mut self, samples: &mut [f32], strength: f32, mut on_prob: impl FnMut(f32)) {
    self.input.push(samples);

    let mut temp_out = [0.0f32; FRAME_SIZE];
    let mut temp_in = [0.0f32; FRAME_SIZE];
    // Linear mix: the default 0.75 keeps 25 % dry, so RNNoise takes at most ~12 dB off any
    // noise. Kept on purpose. A dB mix (dry 6 dB down to 20 / 30 dB, `private_report`,
    // auto + lookahead, 2026-11-08) made pauses 19 / 25 dB quieter instead of 12-14, but the
    // soft syllables the gate cuts rose 1.6-2.4 -> 2.6-4.3 / 3.8-5.9 %. A few keys let through
    // beat one cut letter. A stricter output detector for RNNoise (0.3 -> 0.5: keys open the
    // gate 18 % instead of 31 %) cost +0.3-0.4 points too and was dropped as well.
    let (wet_gain, dry_gain) = (strength, 1.0 - strength);

    while let Some(frame) = self.input.window() {
      for (dst, src) in temp_in.iter_mut().zip(frame) {
        *dst = src * I16_SCALE;
      }
      self.input.advance();

      on_prob(self.denoiser.process_frame(&mut temp_out, &temp_in));

      for (wet, dry) in temp_out.iter().zip(self.prev_dry.iter()) {
        self.output.push((wet_gain * wet + dry_gain * dry) / I16_SCALE);
      }
      self.prev_dry = temp_in;
    }

    self.output.drain_into(samples);
  }

  pub(super) fn reset(&mut self) {
    self.denoiser = DenoiseState::new();
    self.restart();
  }

  /// `restart`, and the audio the network still holds (input filter, window overlap, pitch
  /// history) pushed out by silence, without a new state (`DenoiseState::new` and even a clone
  /// allocate). The recurrent state stays about as warm as it was.
  pub(super) fn flush(&mut self) {
    let (silence, mut out) = ([0.0f32; FRAME_SIZE], [0.0f32; FRAME_SIZE]);
    for _ in 0..RNNOISE_FLUSH_FRAMES {
      self.denoiser.process_frame(&mut out, &silence);
    }
    self.restart();
  }

  /// Fresh buffers, the network state kept: a cold RNNoise rates the first frames of typing as
  /// speech, so a fresh one at every pause woke the heavy model again (gate open 6-7 % on keys).
  pub(super) fn restart(&mut self) {
    self.input.clear();
    self.output.clear();
    self.prev_dry = [0.0; FRAME_SIZE];
  }
}
