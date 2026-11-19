//! Pure-Rust port of Silero VAD v6 (snakers4/silero-vad, tag v6.2.3, the 16 kHz network): a
//! voice detector that returns one speech probability per 32 ms. See README.md.
//!
//! Network: STFT magnitudes (256 / 128, 4 frames per chunk) -> four 1-D convolutions with ReLU
//! -> one LSTM cell -> ReLU, 1x1 convolution, sigmoid.

mod weights;

pub use weights::WeightsError;

use std::sync::OnceLock;
use weights::{Conv, Weights, BINS, CONVS, FFT, HIDDEN};

/// The network's sample rate.
pub const SAMPLE_RATE: usize = 16_000;
/// Samples per `process_chunk` call (32 ms at 16 kHz).
pub const CHUNK: usize = 512;
/// Samples of the previous chunk the network sees again in front of each chunk.
const CONTEXT: usize = 64;
const HOP: usize = 128;
/// STFT frames per chunk: the context, the chunk and `CONTEXT` reflected samples behind it.
const FRAMES: usize = (CONTEXT + CHUNK + CONTEXT - FFT) / HOP + 1;
const PADDED: usize = CONTEXT + CHUNK + CONTEXT;
/// Widest activation of any layer (values per chunk): the magnitudes.
const WIDEST: usize = FRAMES * BINS;

/// 48 kHz -> 16 kHz low-pass: windowed sinc, cutoff 7.2 kHz. Delay `(DECIMATOR_TAPS - 1) / 2`
/// input samples (0.65 ms).
const DECIMATOR_TAPS: usize = 63;
const DECIMATOR_CUTOFF: f32 = 7_200.0 / 48_000.0;
/// 48 kHz samples per chunk.
pub const CHUNK_48K: usize = CHUNK * 3;

const BLOB: &[u8] = include_bytes!("../models/silero_vad_v6.bin");
/// The parsed blob (1.2 MB), once per process: every stream of the audio engine may run a
/// detector, and a copy each would cost that much per stream.
static WEIGHTS: OnceLock<Result<Weights, WeightsError>> = OnceLock::new();

/// One detector instance at 16 kHz: about 10 KB of state, the weights are shared. No heap
/// allocation after construction.
pub struct SileroVad {
  weights: &'static Weights,
  hidden: [f32; HIDDEN],
  cell: [f32; HIDDEN],
  context: [f32; CONTEXT],
  padded: [f32; PADDED],
  /// Layer activations, `[frame][channel]`, swapped after each layer.
  act: [f32; WIDEST],
  next: [f32; WIDEST],
  gates: [f32; 4 * HIDDEN],
}

impl SileroVad {
  /// A corrupt weight blob is an `Err`, never a panic.
  pub fn new() -> Result<Box<Self>, WeightsError> {
    Ok(Box::new(Self {
      weights: WEIGHTS.get_or_init(|| Weights::parse(BLOB)).as_ref().map_err(Clone::clone)?,
      hidden: [0.0; HIDDEN],
      cell: [0.0; HIDDEN],
      context: [0.0; CONTEXT],
      padded: [0.0; PADDED],
      act: [0.0; WIDEST],
      next: [0.0; WIDEST],
      gates: [0.0; 4 * HIDDEN],
    }))
  }

  /// Speech probability (0..1) of the next `CHUNK` samples at 16 kHz, in the -1..1 range. A
  /// shorter slice is zero-padded, a longer one cut, NaN and infinite samples count as 0.
  pub fn process_chunk(&mut self, chunk: &[f32]) -> f32 {
    let Self { weights, hidden, cell, context, padded, act, next, gates } = self;
    // Input: context, chunk, then the last `CONTEXT` samples mirrored (reflect padding).
    padded.fill(0.0);
    let (head, body) = padded.split_at_mut(CONTEXT);
    head.copy_from_slice(context);
    for (dst, &src) in body.iter_mut().zip(chunk.iter().take(CHUNK)) {
      *dst = if src.is_finite() { src } else { 0.0 };
    }
    let (signal, tail) = padded.split_at_mut(CONTEXT + CHUNK);
    for (dst, &src) in tail.iter_mut().zip(signal.iter().rev().skip(1)) {
      *dst = src;
    }
    if let Some(last) = signal.last_chunk::<CONTEXT>() {
      *context = *last;
    }

    // STFT magnitudes.
    let (real, imag) = weights.stft.split_at(BINS * FFT);
    for (frame, mags) in padded.windows(FFT).step_by(HOP).zip(act.as_chunks_mut::<BINS>().0) {
      for ((mag, re), im) in mags.iter_mut().zip(real.as_chunks::<FFT>().0).zip(imag.as_chunks::<FFT>().0) {
        *mag = dot(re, frame).hypot(dot(im, frame));
      }
    }

    let mut frames = FRAMES;
    for conv in &weights.convs {
      frames = convolve(conv, act, frames, next);
      std::mem::swap(act, next);
    }
    // One frame of `HIDDEN` values is left: the LSTM cell's input.
    let Some(input) = act.first_chunk::<HIDDEN>().filter(|_| frames == 1) else {
      return 0.0;
    };
    for (((gate, wi), wh), bias) in gates
      .iter_mut()
      .zip(weights.lstm_input.as_chunks::<HIDDEN>().0)
      .zip(weights.lstm_hidden.as_chunks::<HIDDEN>().0)
      .zip(&weights.lstm_bias)
    {
      *gate = dot(wi, input) + dot(wh, hidden.as_slice()) + bias;
    }
    let (in_gate, rest) = gates.split_at(HIDDEN);
    let (forget, rest) = rest.split_at(HIDDEN);
    let (candidate, out_gate) = rest.split_at(HIDDEN);
    let mut logit = weights.out_bias;
    for (i, (h, c)) in hidden.iter_mut().zip(cell.iter_mut()).enumerate() {
      let at = |gate: &[f32]| gate.get(i).copied().unwrap_or_default();
      *c = sigmoid(at(forget)) * *c + sigmoid(at(in_gate)) * at(candidate).tanh();
      *h = sigmoid(at(out_gate)) * c.tanh();
      logit += weights.out_weight.get(i).copied().unwrap_or_default() * h.max(0.0);
    }
    let prob = sigmoid(logit);
    if !prob.is_finite() {
      self.reset(); // cannot happen with finite input and weights; must not stick if it does
      return 0.0;
    }
    prob
  }

  /// Fresh state (a new stream). No allocation.
  pub fn reset(&mut self) {
    self.hidden = [0.0; HIDDEN];
    self.cell = [0.0; HIDDEN];
    self.context = [0.0; CONTEXT];
  }
}

/// `SileroVad` fed with 48 kHz audio in blocks of any size.
pub struct Vad48k {
  vad: Box<SileroVad>,
  taps: [f32; DECIMATOR_TAPS],
  /// The last `DECIMATOR_TAPS` inputs, written twice so they are always one contiguous slice.
  ring: [f32; 2 * DECIMATOR_TAPS],
  pos: usize,
  /// Inputs since the last 16 kHz sample (0..3).
  phase: usize,
  chunk: [f32; CHUNK],
  filled: usize,
}

impl Vad48k {
  pub fn new() -> Result<Self, WeightsError> {
    let mut taps = [0.0f32; DECIMATOR_TAPS];
    let mid = (DECIMATOR_TAPS - 1) as f32 / 2.0;
    for (n, tap) in taps.iter_mut().enumerate() {
      let x = n as f32 - mid;
      let sinc = if x == 0.0 {
        1.0
      } else {
        (std::f32::consts::TAU * DECIMATOR_CUTOFF * x).sin() / (std::f32::consts::TAU * DECIMATOR_CUTOFF * x)
      };
      let hann = 0.5 - 0.5 * (std::f32::consts::TAU * (n as f32 + 0.5) / DECIMATOR_TAPS as f32).cos();
      *tap = sinc * hann;
    }
    let sum: f32 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= sum);
    Ok(Self {
      vad: SileroVad::new()?,
      taps,
      ring: [0.0; 2 * DECIMATOR_TAPS],
      pos: 0,
      phase: 0,
      chunk: [0.0; CHUNK],
      filled: 0,
    })
  }

  /// Feeds 48 kHz samples (-1..1); `on_prob` gets the speech probability of each 32 ms chunk
  /// (`CHUNK_48K` samples) completed in this block. No allocation.
  pub fn process(&mut self, samples: &[f32], mut on_prob: impl FnMut(f32)) {
    for &s in samples {
      for at in [self.pos, self.pos + DECIMATOR_TAPS] {
        if let Some(slot) = self.ring.get_mut(at) {
          *slot = s;
        }
      }
      self.pos = (self.pos + 1) % DECIMATOR_TAPS;
      self.phase += 1;
      if self.phase < 3 {
        continue;
      }
      self.phase = 0;
      let window = self.ring.get(self.pos..self.pos + DECIMATOR_TAPS).unwrap_or_default();
      if let Some(slot) = self.chunk.get_mut(self.filled) {
        *slot = dot(&self.taps, window);
      }
      self.filled += 1;
      if self.filled == CHUNK {
        self.filled = 0;
        on_prob(self.vad.process_chunk(&self.chunk));
      }
    }
  }

  /// Fresh state (a new stream): the network and the buffered audio. No allocation.
  pub fn reset(&mut self) {
    self.vad.reset();
    self.ring = [0.0; 2 * DECIMATOR_TAPS];
    (self.pos, self.phase, self.filled) = (0, 0, 0);
  }

  /// Drops the buffered audio and keeps the network's state (a pause in the stream).
  pub fn flush(&mut self) {
    self.ring = [0.0; 2 * DECIMATOR_TAPS];
    (self.pos, self.phase, self.filled) = (0, 0, 0);
  }
}

/// Kernel 3, padding 1, then ReLU. `input` and `output` are `[frame][channel]`; returns the
/// number of output frames.
fn convolve(conv: &Conv, input: &[f32], frames: usize, output: &mut [f32]) -> usize {
  let Conv { weight, bias, inputs, stride } = conv;
  let (inputs, stride, outputs) = (*inputs, (*stride).max(1), bias.len());
  let out_frames = (frames + 2 - 3) / stride + 1;
  for (t, out) in output.chunks_exact_mut(outputs).take(out_frames).enumerate() {
    for ((o, w), b) in out.iter_mut().zip(weight.chunks_exact(3 * inputs)).zip(bias) {
      let mut sum = *b;
      for (k, taps) in w.chunks_exact(inputs).enumerate() {
        // Input frame of this tap; outside the signal is the zero padding.
        let Some(frame) = (t * stride + k).checked_sub(1).filter(|&f| f < frames) else {
          continue;
        };
        sum += dot(taps, input.get(frame * inputs..(frame + 1) * inputs).unwrap_or_default());
      }
      *o = sum.max(0.0);
    }
  }
  out_frames
}

/// Eight running sums so the compiler can vectorize it (a single sum must stay in order).
fn dot(a: &[f32], b: &[f32]) -> f32 {
  let mut acc = [0.0f32; 8];
  let (a8, b8) = (a.as_chunks::<8>(), b.as_chunks::<8>());
  for (x, y) in a8.0.iter().zip(b8.0) {
    for ((s, x), y) in acc.iter_mut().zip(x).zip(y) {
      *s += x * y;
    }
  }
  let tail: f32 = a8.1.iter().zip(b8.1).map(|(x, y)| x * y).sum();
  acc.iter().sum::<f32>() + tail
}

fn sigmoid(x: f32) -> f32 {
  1.0 / (1.0 + (-x).exp())
}

// The compile-time shape the code above relies on: the encoder ends in one frame of `HIDDEN`.
const _: () = assert!(FRAMES == 4 && CONVS[3].1 == HIDDEN && WIDEST >= FRAMES * CONVS[0].1);
