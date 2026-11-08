//! Runs the 48 kHz-only mic chain on a stream at another rate (44.1 kHz USB mics, 16/8 kHz
//! Bluetooth headsets, 96/192 kHz interfaces): device rate -> 48 kHz -> chain -> device rate.
//!
//! Both directions use rubato's streaming sinc resampler (asynchronous, fixed input),
//! fed in fixed 2 ms chunks through small FIFOs, so the engine's block size can be anything.
//! The output FIFO starts primed with the worst-case shortfall, so every block gets exactly its
//! own length back. Added latency: ~4 ms of chunking plus the sinc filters (~1 ms).
//!
//! Real-time safe in steady state: all buffers are preallocated for blocks up to
//! `PREALLOC_FRAMES`; a larger engine block grows them once (allocation in the audio thread).

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
  Async, FixedAsync, ResampleError, Resampler, ResamplerConstructionError, SincInterpolationParameters,
  SincInterpolationType, WindowFunction,
};

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// Rate the chain runs at (the reference ring's rate).
pub const CHAIN_RATE: u32 = 48_000;
/// Chunk length fed to each resampler. Shorter = less latency, more per-call overhead.
const CHUNK_MS: u32 = 2;
const SINC_LEN: usize = 64;
const OVERSAMPLING: usize = 128;
const WINDOW: WindowFunction = WindowFunction::BlackmanHarris2;
/// Covers an 85 ms engine period at 192 kHz without growing any buffer.
const PREALLOC_FRAMES: usize = 16_384;
/// Rates the bridge accepts; outside it the APO passes audio through.
const MIN_RATE: u32 = 8_000;
const MAX_RATE: u32 = 384_000;
const HNS_PER_SEC: u64 = 10_000_000;

// ---------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------

/// Mono resampling sandwich around a chain that only runs at `CHAIN_RATE`.
pub struct RateBridge {
  device_rate: u32,
  up: Stage,
  down: Stage,
  /// One call's chain input/output (48 kHz). Leftovers short of a chunk wait in the stages.
  chain_in: Vec<f32>,
  chain_out: Vec<f32>,
  /// Device-rate output ready to hand back, primed with silence.
  out_fifo: Vec<f32>,
  /// Cap on what stays in `out_fifo` after a block (2x the prime); excess = drift, dropped.
  out_limit: usize,
  /// Silence `out_fifo` starts with (the pipeline latency).
  prime: usize,
  /// Blocks that came back partly silent because the pipeline was short (should stay 0).
  underruns: u64,
}

/// One resampler with its input FIFO and output scratch.
struct Stage {
  resampler: Async<f32>,
  chunk: usize,
  /// Output frames still to drop: the sinc filter's delay, once after a start or reset.
  /// (rubato < 1 withheld it itself; 5 puts it at the front of the output, which added up to
  /// 4 ms at 8 kHz.)
  skip: usize,
  fifo: Vec<f32>,
  scratch: Vec<f32>,
}

// ---------------------------------------------------------------------------------------
// Constructors and public functions
// ---------------------------------------------------------------------------------------

impl RateBridge {
  /// None at `CHAIN_RATE` (no bridge needed) or outside the supported range.
  pub fn for_rate(device_rate: u32) -> Option<Result<Self, ResamplerConstructionError>> {
    (device_rate != CHAIN_RATE && (MIN_RATE..=MAX_RATE).contains(&device_rate)).then(|| Self::new(device_rate))
  }

  pub fn new(device_rate: u32) -> Result<Self, ResamplerConstructionError> {
    let up = Stage::new(device_rate, CHAIN_RATE)?;
    let down = Stage::new(CHAIN_RATE, device_rate)?;
    // Worst case short of output: a device chunk stuck in `up`, a 48 kHz chunk stuck in
    // `down`, the frames each sinc filter withholds at start-up (its delay), plus rounding of
    // the fractional resampler positions. 48 kHz counts are converted to device frames.
    let to_device = |frames: usize| (frames as u64 * device_rate as u64).div_ceil(CHAIN_RATE as u64) as usize;
    let prime =
      up.chunk + to_device(down.chunk + up.resampler.output_delay()) + down.resampler.output_delay() + 2;
    let mut out_fifo = Vec::with_capacity(2 * prime + PREALLOC_FRAMES);
    out_fifo.resize(prime, 0.0);
    Ok(Self {
      device_rate,
      up,
      down,
      chain_in: Vec::with_capacity(2 * PREALLOC_FRAMES),
      chain_out: Vec::with_capacity(2 * PREALLOC_FRAMES),
      out_fifo,
      out_limit: 2 * prime,
      prime,
      underruns: 0,
    })
  }

  /// Back to the state of `new` without allocating: after a pause the FIFOs hold audio from
  /// before it (~5 ms that would play first, out of place).
  pub fn reset(&mut self) {
    for stage in [&mut self.up, &mut self.down] {
      stage.resampler.reset();
      stage.fifo.clear();
      stage.skip = stage.resampler.output_delay();
    }
    self.out_fifo.clear();
    self.out_fifo.resize(self.prime, 0.0);
  }

  /// Replaces `block` (device rate, mono) with the chain's output for it, delayed by the
  /// bridge latency. `time_hns` is the capture time of `block[0]`; `chain` gets 48 kHz input,
  /// an equally long output slice, and the capture time of its first input frame.
  pub fn process(
    &mut self,
    block: &mut [f32],
    time_hns: Option<u64>,
    mut chain: impl FnMut(&[f32], &mut [f32], Option<u64>),
  ) -> Result<(), ResampleError> {
    // Frames already queued in `up` are older than `block[0]`; the first chunk starts there.
    let queued_hns = self.up.fifo.len() as u64 * HNS_PER_SEC / self.device_rate as u64;
    let chain_time = time_hns.map(|t| t.saturating_sub(queued_hns));

    self.up.fifo.extend_from_slice(block);
    self.chain_in.clear();
    self.up.drain_into(&mut self.chain_in)?;

    if !self.chain_in.is_empty() {
      self.chain_out.clear();
      self.chain_out.resize(self.chain_in.len(), 0.0);
      chain(&self.chain_in, &mut self.chain_out, chain_time);
      self.down.fifo.extend_from_slice(&self.chain_out);
    }
    self.down.drain_into(&mut self.out_fifo)?;

    let ready = self.out_fifo.len().min(block.len());
    if ready < block.len() {
      // Only if the prime was too small: silence in front keeps the stream continuous.
      self.underruns += 1;
    }
    let (silence, rest) = block.split_at_mut(block.len() - ready);
    silence.fill(0.0);
    for (dst, src) in rest.iter_mut().zip(self.out_fifo.drain(..ready)) {
      *dst = src;
    }
    if self.out_fifo.len() > self.out_limit {
      // Only on drift (should not happen with exact ratios): drop the oldest to bound latency.
      let excess = self.out_fifo.len() - self.out_limit;
      self.out_fifo.drain(..excess);
    }
    Ok(())
  }
}

impl Stage {
  fn new(rate_in: u32, rate_out: u32) -> Result<Self, ResamplerConstructionError> {
    let params = SincInterpolationParameters {
      sinc_len: SINC_LEN,
      f_cutoff: None, // rubato picks it from SINC_LEN and WINDOW (calculate_cutoff)
      oversampling_factor: OVERSAMPLING,
      interpolation: SincInterpolationType::Linear,
      window: WINDOW,
    };
    let chunk = (rate_in / 1000 * CHUNK_MS).max(1) as usize;
    let resampler = Async::new_sinc(rate_out as f64 / rate_in as f64, 1.0, &params, chunk, 1, FixedAsync::Input)?;
    let scratch = vec![0.0; resampler.output_frames_max()];
    let skip = resampler.output_delay();
    Ok(Self { resampler, chunk, skip, fifo: Vec::with_capacity(chunk + PREALLOC_FRAMES), scratch })
  }

  /// Resamples every full chunk in `fifo` and appends the result to `out`.
  fn drain_into(&mut self, out: &mut Vec<f32>) -> Result<(), ResampleError> {
    let mut consumed = 0;
    while let Some(chunk) = self.fifo.get(consumed..consumed + self.chunk) {
      // Mono: interleaved = a plain slice. The sizes always fit (`chunk` is exactly `self.chunk`
      // frames, scratch holds `output_frames_max`), so the adapters can't fail; if they did, stop.
      let frames_out = self.scratch.len();
      let (Ok(input), Ok(mut output)) =
        (InterleavedSlice::new(chunk, 1, self.chunk), InterleavedSlice::new_mut(&mut self.scratch, 1, frames_out))
      else {
        break;
      };
      let (_, produced) = self.resampler.process_into_buffer(&input, &mut output, None)?;
      let dropped = self.skip.min(produced);
      self.skip -= dropped;
      out.extend_from_slice(self.scratch.get(dropped..produced).unwrap_or(&[]));
      consumed += self.chunk;
    }
    self.fifo.drain(..consumed);
    Ok(())
  }
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
  use super::*;

  const AMP: f32 = 0.3;

  #[test]
  fn identity_chain_returns_the_same_tone_at_every_rate() {
    for rate in [8_000, 16_000, 22_050, 32_000, 44_100, 88_200, 96_000, 192_000] {
      let (input, output, underruns) = run(rate, rate as usize / 100, |i, o, _| o.copy_from_slice(i));
      assert_eq!(underruns, 0, "{rate} Hz: pipeline ran short");
      // Latency: where the output first reaches half the tone's amplitude, against the input.
      let onset = |x: &[f32]| x.iter().position(|s| s.abs() >= AMP / 2.0).unwrap();
      let lag = onset(&output) - onset(&input);
      assert!(lag < rate as usize / 100, "{rate} Hz: latency {lag} frames over 10 ms");
      // Distortion: fit a 440 Hz sine to the settled output (any phase = any fractional delay).
      let settled = &output[rate as usize / 10..];
      let (amp, snr_db) = sine_fit(settled, rate, 440.0);
      let gain_db = 20.0 * (amp / AMP as f64).log10();
      assert!(gain_db.abs() < 0.2, "{rate} Hz: gain {gain_db:.2} dB");
      assert!(snr_db > 60.0, "{rate} Hz: round trip SNR {snr_db:.1} dB");
    }
  }

  #[test]
  fn chain_runs_at_48k_with_steady_timestamps() {
    let mut bridge = RateBridge::new(44_100).unwrap();
    let mut total = 0usize;
    let mut calls: Vec<(u64, usize)> = Vec::new();
    let start = 1_000_000_000u64;
    let mut device_frames = 0u64;
    // Odd, varying block sizes: the bridge must still return exactly each block's length.
    for (i, size) in [441usize, 7, 1000, 441, 300].iter().cycle().take(400).enumerate() {
      let mut block = tone(44_100, device_frames as usize, *size);
      let t = start + device_frames * HNS_PER_SEC / 44_100;
      bridge
        .process(&mut block, Some(t), |inp, out, time| {
          total += inp.len();
          calls.push((time.unwrap(), inp.len()));
          out.copy_from_slice(inp);
        })
        .unwrap();
      assert_eq!(block.len(), *size, "block {i}");
      device_frames += *size as u64;
    }
    assert_eq!(bridge.underruns, 0);
    let expected = device_frames as f64 * 48_000.0 / 44_100.0;
    assert!((total as f64 - expected).abs() < 200.0, "chain got {total} frames, expected ~{expected:.0}");
    // Each call starts where the previous one ended (48 kHz clock), within 0.1 ms. The first
    // call is short (the sinc filter withholds its delay once), so start after it.
    for pair in calls.windows(2).skip(1) {
      let ((t0, n0), (t1, _)) = (pair[0], pair[1]);
      let step = t1 as i64 - t0 as i64;
      let want = (n0 as u64 * HNS_PER_SEC / 48_000) as i64;
      assert!((step - want).abs() < 1_000, "step {step} hns, expected {want}");
    }
  }

  #[test]
  fn no_bridge_at_48k_or_outside_the_supported_range() {
    assert!(RateBridge::for_rate(48_000).is_none());
    assert!(RateBridge::for_rate(4_000).is_none());
    assert!(RateBridge::for_rate(768_000).is_none());
    assert!(RateBridge::for_rate(16_000).unwrap().is_ok());
  }

  /// Feeds 1 s of a 440 Hz tone in `block`-sized pieces; returns (input, output, underruns).
  fn run(rate: u32, block: usize, mut chain: impl FnMut(&[f32], &mut [f32], Option<u64>)) -> (Vec<f32>, Vec<f32>, u64) {
    let mut bridge = RateBridge::new(rate).unwrap();
    let input = tone(rate, 0, rate as usize);
    let mut output = Vec::with_capacity(input.len());
    for piece in input.chunks(block) {
      let mut buf = piece.to_vec();
      bridge.process(&mut buf, None, &mut chain).unwrap();
      assert_eq!(buf.len(), piece.len());
      assert!(buf.iter().all(|s| s.is_finite()));
      output.extend_from_slice(&buf);
    }
    (input, output, bridge.underruns)
  }

  fn tone(rate: u32, offset: usize, frames: usize) -> Vec<f32> {
    (offset..offset + frames).map(|n| (n as f32 * 2.0 * std::f32::consts::PI * 440.0 / rate as f32).sin() * AMP).collect()
  }

  /// Least-squares fit of a `freq` sine (any phase) to `x`: (amplitude, fit-to-residual dB).
  fn sine_fit(x: &[f32], rate: u32, freq: f64) -> (f64, f64) {
    let w = 2.0 * std::f64::consts::PI * freq / rate as f64;
    let (mut ss, mut cc, mut sc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (n, &v) in x.iter().enumerate() {
      let (s, c) = (w * n as f64).sin_cos();
      let v = v as f64;
      ss += s * s;
      cc += c * c;
      sc += s * c;
      xs += v * s;
      xc += v * c;
    }
    let det = ss * cc - sc * sc;
    let (a, b) = ((xs * cc - xc * sc) / det, (xc * ss - xs * sc) / det);
    let (mut fit, mut res) = (0.0, 0.0);
    for (n, &v) in x.iter().enumerate() {
      let (s, c) = (w * n as f64).sin_cos();
      let model = a * s + b * c;
      fit += model * model;
      res += (v as f64 - model).powi(2);
    }
    ((a * a + b * b).sqrt(), 10.0 * (fit / res).log10())
  }
}
