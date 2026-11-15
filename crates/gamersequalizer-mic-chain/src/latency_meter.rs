//! Measures the delay the mic chain really adds: GCC-PHAT between the raw input and the
//! final output (the method `DelayEstimator` uses for the echo), at 8 kHz, every ~0.5 s of
//! output. Whatever the stages do (AEC framing, model, lookahead, buffering after an odd
//! block size), the speech in the output is the input's, so the correlation peak sits at the
//! actual delay. Windows without a clear peak (silence, gate closed on noise) are skipped and
//! the last measurement stays.
//!
//! Known limit: measured inside the 48 kHz chain. The APO's resampler for devices at other
//! rates (`rate_bridge`) is outside it.

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// 48 kHz -> 8 kHz.
const DS: usize = 6;
/// Output per measurement: 4096 samples at 8 kHz = 0.51 s.
const WINDOW: usize = 4096;
/// Longest delay searched: 200 ms (the heaviest chain is ~90 ms).
const MAX_LAG: usize = 1600;
/// Circular correlation is alias-free for lags 0..=MAX_LAG with this length.
const FFT_LEN: usize = 8192;
/// Input history kept: the output window plus the search range.
const HISTORY: usize = WINDOW + MAX_LAG;
/// Only speech bins count (the EQ's low/high cut leave the rest uncorrelated): 150-3800 Hz.
const FIRST_BIN: usize = 150 * FFT_LEN / 8_000;
const LAST_BIN: usize = 3_800 * FFT_LEN / 8_000;
/// Peak-to-average ratio of the PHAT correlation below which a window says nothing.
const MIN_PEAK_RATIO: f32 = 12.0;
/// Output RMS (8 kHz) under which a window is skipped: about -66 dBFS.
const MIN_OUT_RMS: f32 = 0.0005;
/// Two windows in a row within this (1 ms, 48 kHz samples) make a measurement: one window can
/// peak on a stray correlation (speech tail into silence read 18 ms for an 79 ms chain).
const AGREE_SAMPLES: f32 = 48.0;

pub struct LatencyMeter {
  fft: Arc<dyn Fft<f32>>,
  ifft: Arc<dyn Fft<f32>>,
  scratch: Vec<Complex32>,
  spec_in: Vec<Complex32>,
  spec_out: Vec<Complex32>,
  /// Downsampled input, circular, `in_head` = next write.
  input: Vec<f32>,
  in_head: usize,
  /// Input samples seen since the last reset (saturates at HISTORY).
  in_filled: usize,
  in_acc: (f32, usize),
  output: Vec<f32>,
  out_acc: (f32, usize),
  measured: Option<f32>,
  /// Last window's estimate, waiting for the next one to agree.
  candidate: Option<f32>,
}

impl Default for LatencyMeter {
  fn default() -> Self {
    Self::new()
  }
}

impl LatencyMeter {
  pub fn new() -> Self {
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(FFT_LEN);
    let ifft = planner.plan_fft_inverse(FFT_LEN);
    let scratch_len = fft.get_inplace_scratch_len().max(ifft.get_inplace_scratch_len());
    let zero = Complex32::new(0.0, 0.0);
    Self {
      fft,
      ifft,
      scratch: vec![zero; scratch_len],
      spec_in: vec![zero; FFT_LEN],
      spec_out: vec![zero; FFT_LEN],
      input: vec![0.0; HISTORY],
      in_head: 0,
      in_filled: 0,
      in_acc: (0.0, 0),
      // A window plus one block of slack (blocks up to ~85 ms) without growing.
      output: Vec::with_capacity(WINDOW + 4096 / DS * 2),
      out_acc: (0.0, 0),
      measured: None,
      candidate: None,
    }
  }

  /// Raw mic block, before any stage. Call once per block, before `push_output`.
  pub fn push_input(&mut self, block: &[f32]) {
    for &s in block {
      if let Some(v) = downsample(&mut self.in_acc, s) {
        if let Some(slot) = self.input.get_mut(self.in_head) {
          *slot = v;
        }
        self.in_head = (self.in_head + 1) % HISTORY;
        self.in_filled = (self.in_filled + 1).min(HISTORY);
      }
    }
  }

  /// Final output block (same length as the input block). Measures once a window is full,
  /// at the block's end: only there do input history and output end at the same instant.
  pub fn push_output(&mut self, block: &[f32]) {
    for &s in block {
      if let Some(v) = downsample(&mut self.out_acc, s) {
        self.output.push(v);
      }
    }
    if self.output.len() >= WINDOW {
      self.measure();
      self.output.clear();
    }
  }

  /// A gap in the stream: input and output no longer line up. Keeps the last measurement.
  pub fn reset(&mut self) {
    self.in_head = 0;
    self.in_filled = 0;
    self.in_acc = (0.0, 0);
    self.output.clear();
    self.out_acc = (0.0, 0);
    self.candidate = None;
  }

  /// The chain changed (model, lookahead, AEC on/off): the old measurement no longer holds.
  /// Forget it and measure afresh from the next block.
  pub fn invalidate(&mut self) {
    self.reset();
    self.measured = None;
    self.candidate = None;
  }

  /// Last measured delay, 48 kHz samples (None until the first clear window).
  pub fn measured_samples(&self) -> Option<f32> {
    self.measured
  }

  fn measure(&mut self) {
    // The input history must reach MAX_LAG before the window's first output sample.
    if self.in_filled < HISTORY {
      return;
    }
    // The newest WINDOW output samples end with the newest input sample.
    let out = self.output.get(self.output.len().saturating_sub(WINDOW)..).unwrap_or_default();
    let out_energy: f32 = out.iter().map(|v| v * v).sum();
    if (out_energy / WINDOW as f32).sqrt() < MIN_OUT_RMS {
      return;
    }
    // Input oldest-first: in[MAX_LAG] lines up with output[0], so output[i] ~ in[i + MAX_LAG - d].
    for (j, slot) in self.spec_in.iter_mut().enumerate() {
      let v = if j < HISTORY { self.input.get((self.in_head + j) % HISTORY).copied().unwrap_or(0.0) } else { 0.0 };
      *slot = Complex32::new(v, 0.0);
    }
    for (j, slot) in self.spec_out.iter_mut().enumerate() {
      *slot = Complex32::new(out.get(j).copied().unwrap_or(0.0), 0.0);
    }
    self.fft.process_with_scratch(&mut self.spec_out, &mut self.scratch);
    self.fft.process_with_scratch(&mut self.spec_in, &mut self.scratch);
    for (k, (o, i)) in self.spec_out.iter_mut().zip(self.spec_in.iter()).enumerate() {
      // Real signals: bin k and FFT_LEN - k mirror each other, both keep the speech band.
      let band = (FIRST_BIN..=LAST_BIN).contains(&k.min(FFT_LEN - k));
      let g = o.conj() * i;
      let mag = g.norm();
      *o = if band && mag > 1e-12 { g / mag } else { Complex32::new(0.0, 0.0) };
    }
    self.ifft.process_with_scratch(&mut self.spec_out, &mut self.scratch);

    // c[k] = sum_i out[i] * in[i + k]; delay d sits at k = MAX_LAG - d.
    let corr = |d: usize| self.spec_out.get(MAX_LAG - d.min(MAX_LAG)).map_or(0.0, |c| c.re);
    let (mut best_d, mut best, mut sum_abs) = (0usize, f32::MIN, 0.0f32);
    for d in 0..=MAX_LAG {
      let v = corr(d);
      sum_abs += v.abs();
      if v > best {
        (best, best_d) = (v, d);
      }
    }
    let mean_abs = sum_abs / (MAX_LAG + 1) as f32;
    if mean_abs <= 1e-12 || best / mean_abs < MIN_PEAK_RATIO {
      return;
    }
    let y0 = if best_d > 0 { corr(best_d - 1) } else { best };
    let y2 = if best_d < MAX_LAG { corr(best_d + 1) } else { best };
    let denom = y0 - 2.0 * best + y2;
    let frac = if denom.abs() > 1e-12 { (0.5 * (y0 - y2) / denom).clamp(-0.5, 0.5) } else { 0.0 };
    let estimate = ((best_d as f32 + frac) * DS as f32).max(0.0);
    if self.candidate.is_some_and(|c| (c - estimate).abs() <= AGREE_SAMPLES) {
      self.measured = Some(estimate);
    }
    self.candidate = Some(estimate);
  }
}

/// Box average of DS samples (the same crude anti-alias as `DelayEstimator`).
fn downsample(acc: &mut (f32, usize), s: f32) -> Option<f32> {
  acc.0 += if s.is_finite() { s } else { 0.0 };
  acc.1 += 1;
  if acc.1 < DS {
    return None;
  }
  let v = acc.0 / DS as f32;
  *acc = (0.0, 0);
  Some(v)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::aec3::{AEC3_LATENCY_SAMPLES, EchoCanceller3};
  use crate::ai_denoiser::AiVoiceIsolator;
  use gamersequalizer_common::shm::DenoiseModel;

  /// A plain delay is found to within one 8 kHz sample, and a gap keeps the old value.
  #[test]
  fn test_measures_a_known_delay() {
    let x = speechy(3 * 48_000);
    let delay = 2_117;
    let mut meter = LatencyMeter::new();
    for (n, block) in x.chunks(480).enumerate() {
      let out: Vec<f32> = (0..block.len()).map(|i| delayed(&x, n * 480 + i, delay) * 0.3).collect();
      meter.push_input(block);
      meter.push_output(&out);
    }
    let got = meter.measured_samples().unwrap_or(f32::NAN);
    assert!((got - delay as f32).abs() <= DS as f32, "measured {got} vs {delay}");
    meter.reset();
    assert_eq!(meter.measured_samples(), Some(got));
  }

  /// Silence measures nothing (no made-up number).
  #[test]
  fn test_silence_reports_none() {
    let mut meter = LatencyMeter::new();
    for _ in 0..400 {
      meter.push_input(&[0.0; 480]);
      meter.push_output(&[0.0; 480]);
    }
    assert_eq!(meter.measured_samples(), None);
  }

  /// The real stages (AEC3 + each model + gate lookahead) on speech: the meter finds the sum
  /// of their latencies, also after a live model switch.
  #[test]
  fn test_measures_the_real_chain() {
    let mut reader =
      hound::WavReader::open(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test_data/denoise/speech_clean.wav")).unwrap();
    let speech: Vec<f32> = reader.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect();
    let cases = [
      (DenoiseModel::RnNoise, None),
      (DenoiseModel::DeepFilterNet, None),
      (DenoiseModel::DpdfNet, None),
      (DenoiseModel::DpdfNet, Some(DenoiseModel::DeepFilterNet)),
      (DenoiseModel::RnNoise, Some(DenoiseModel::DeepFilterNet)),
    ];
    for (first, switch_to) in cases {
      let mut aec = EchoCanceller3::new();
      let mut iso = AiVoiceIsolator::new();
      iso.set_model(first);
      iso.set_gate_threshold_db(1.0); // auto gate with lookahead
      let mut meter = LatencyMeter::new();
      let silent = [0.0f32; 480];
      for (i, block) in speech.as_chunks::<480>().0.iter().enumerate() {
        if let Some(m) = switch_to.filter(|_| i == 300) {
          // The model loads on its own thread: wait for it (silence meanwhile, not measured), or
          // the speech is over before it takes over and nothing after a switch gets measured.
          // The chain changed, so the old measurement goes, as in the plugin.
          iso.request_model(m);
          let started = std::time::Instant::now();
          while iso.active_model() != m {
            assert!(started.elapsed().as_secs() < 20, "{m:?} never took over");
            std::thread::sleep(std::time::Duration::from_millis(10));
            iso.process_samples(&mut [0.0f32; 480]);
          }
          meter.invalidate();
        }
        meter.push_input(block);
        let mut b = block.to_vec();
        aec.process_block(&silent, &mut b);
        iso.process_samples(&mut b);
        meter.push_output(&b);
      }
      let expected = (AEC3_LATENCY_SAMPLES + iso.output_lead_samples()) as f32;
      let got = meter.measured_samples().unwrap_or(f32::NAN);
      eprintln!(
        "{first:?} -> {switch_to:?}: active {:?}, measured {:.1} ms, stages {:.1} ms",
        iso.active_model(),
        got / 48.0,
        expected / 48.0
      );
      assert!(
        (got - expected).abs() <= 2.0 * DS as f32,
        "{first:?} -> {switch_to:?}: measured {got} vs stages {expected}"
      );
    }
  }

  /// A heavy model sleeps in a pause and wakes on speech: the delay afterwards is its own.
  #[test]
  fn test_measures_after_the_model_slept() {
    let mut reader =
      hound::WavReader::open(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test_data/denoise/speech_clean.wav")).unwrap();
    let speech: Vec<f32> = reader.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect();
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
      let mut input = speech[..3 * 48_000].to_vec();
      input.extend(std::iter::repeat_n(0.0, 4 * 48_000));
      input.extend_from_slice(&speech[3 * 48_000..]);
      let mut aec = EchoCanceller3::new();
      let mut iso = AiVoiceIsolator::new();
      iso.set_model(model);
      iso.set_gate_threshold_db(1.0);
      let mut meter = LatencyMeter::new();
      let (mut slept, mut log) = (false, Vec::new());
      for (i, block) in input.as_chunks::<480>().0.iter().enumerate() {
        meter.push_input(block);
        let mut b = block.to_vec();
        aec.process_block(&[0.0; 480], &mut b);
        iso.process_samples(&mut b);
        meter.push_output(&b);
        slept |= iso.asleep();
        if i % 50 == 0 {
          log.push(format!(
            "{}:{}{:.0}",
            i / 100,
            if iso.asleep() { "z" } else { "" },
            meter.measured_samples().unwrap_or(0.0) / 48.0
          ));
        }
      }
      let expected = (AEC3_LATENCY_SAMPLES + iso.output_lead_samples()) as f32;
      let got = meter.measured_samples().unwrap_or(f32::NAN);
      eprintln!(
        "{model:?}: slept {slept}, measured {:.1} ms, stages {:.1} ms | {}",
        got / 48.0,
        expected / 48.0,
        log.join(" ")
      );
      assert!(slept, "{model:?} never slept");
      assert!((got - expected).abs() <= 2.0 * DS as f32, "{model:?}: measured {got} vs stages {expected}");
    }
  }

  fn delayed(x: &[f32], n: usize, by: usize) -> f32 {
    n.checked_sub(by).and_then(|i| x.get(i)).copied().unwrap_or(0.0)
  }

  /// Noise bursts in 4 Hz syllables (broadband, like speech for the correlation).
  fn speechy(len: usize) -> Vec<f32> {
    let mut s = 7u32;
    (0..len)
      .map(|n| {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let white = (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
        white * (n as f32 * 2.0 * std::f32::consts::PI * 4.0 / 48_000.0).sin().max(0.0)
      })
      .collect()
  }
}
