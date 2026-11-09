//! Pure-Rust port of RNNoise as of xiph/rnnoise `main` @ 70f1d25 (the "0.2" network: conv1d,
//! three GRUs with skip connections, 32 bands), forked from nnnoiseless 0.5.2. See README.md.
//!
//! The interface mirrors nnnoiseless: 480-sample frames at 48 kHz in the i16 range as `f32`.

mod bands;
mod denoise;
mod kernel;
mod nnet;
mod pitch;
mod weights;

pub use weights::WeightsError;

use bands::Tables;
use denoise::{Memory, Transform};
use nnet::RnnModel;

/// Samples per `process_frame` call (10 ms at 48 kHz).
pub const FRAME_SIZE: usize = bands::FRAME_SIZE;

/// Algorithmic delay of `process_frame` in frames: one frame of overlap-add plus one frame
/// because 0.2 applies the gains computed on frame t to frame t-1.
pub const LATENCY_FRAMES: usize = 2;

const FULL_BLOB: &[u8] = include_bytes!("../models/rnnoise_full.bin");
const LITTLE_BLOB: &[u8] = include_bytes!("../models/rnnoise_little.bin");

/// Which embedded weight set to run; both have the same shapes and cost per weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Model {
  /// The full model (dense GRU matrices, 3.5 MB blob).
  #[default]
  Full,
  /// The sparser "little" model (1/3 of the GRU blocks, 1.6 MB blob, about 3x cheaper GRUs).
  Little,
}

impl Model {
  fn blob(self) -> &'static [u8] {
    match self {
      Model::Full => FULL_BLOB,
      Model::Little => LITTLE_BLOB,
    }
  }
}

/// One denoiser instance. Large (tens of KB plus the model tables), so it lives in a `Box`.
/// No heap allocation after construction.
pub struct DenoiseState {
  model: RnnModel,
  tables: Tables,
  fft: Transform,
  memory: Memory,
}

impl DenoiseState {
  /// Samples per `process_frame` call, as `nnnoiseless::DenoiseState::FRAME_SIZE`.
  pub const FRAME_SIZE: usize = FRAME_SIZE;

  /// A denoiser with the `Full` model. Fails only if the embedded weights are corrupt.
  pub fn new() -> Result<Box<Self>, WeightsError> {
    Self::with_model(Model::Full)
  }

  /// A denoiser with the given embedded model; the blob is parsed and checked here, once.
  pub fn with_model(model: Model) -> Result<Box<Self>, WeightsError> {
    Ok(Box::new(DenoiseState {
      model: weights::load_model(model.blob())?,
      tables: Tables::new(),
      fft: Transform::new(),
      memory: Memory::new(),
    }))
  }

  /// Denoises one frame. `input` and `output` hold `FRAME_SIZE` samples in the i16 range
  /// (`-32768.0..=32767.0`); the output lags the input by `LATENCY_FRAMES * FRAME_SIZE` samples.
  /// A shorter `input` is zero-padded, extra samples are ignored; only the first
  /// `FRAME_SIZE` samples of `output` are written. Returns the voice probability (0 in silence).
  pub fn process_frame(&mut self, output: &mut [f32], input: &[f32]) -> f32 {
    let frame_in = frame_in(input);
    let mut frame_out = [0.0f32; FRAME_SIZE];
    let vad =
      denoise::process_frame(&self.model, &self.tables, &mut self.fft, &mut self.memory, &mut frame_out, &frame_in);
    output.iter_mut().zip(frame_out).for_each(|(d, s)| *d = s);
    vad
  }

  /// The voice probability of one frame without producing audio (analysis and network only:
  /// no pitch filter, gains or synthesis), for a voice detector. Input as for `process_frame`
  /// (short input zero-padded, extra samples ignored, non-finite samples zeroed); 0 on silent
  /// frames. On the same input sequence it returns exactly the values `process_frame` returns.
  /// It skips the output-side state, so the first few `process_frame` outputs after switching an
  /// instance from this to `process_frame` are not meaningful (call `reset()` to switch cleanly).
  pub fn voice_probability(&mut self, input: &[f32]) -> f32 {
    denoise::voice_probability(&self.model, &self.tables, &mut self.fft, &mut self.memory, &frame_in(input))
  }

  /// Clears all signal and network state (as a fresh instance), keeping the parsed model.
  pub fn reset(&mut self) {
    self.memory = Memory::new();
  }
}

/// `input` as one frame: zero-padded, and a NaN/Inf sample from the host (which would poison every
/// filter and network state until `reset()`) replaced by 0.
fn frame_in(input: &[f32]) -> [f32; FRAME_SIZE] {
  let mut frame = [0.0f32; FRAME_SIZE];
  frame.iter_mut().zip(input).for_each(|(d, s)| *d = if s.is_finite() { *s } else { 0.0 });
  frame
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn state_can_move_to_the_audio_thread() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DenoiseState>();
  }
}
