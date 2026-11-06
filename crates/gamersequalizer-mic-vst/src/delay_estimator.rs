//! Echo-path delay estimator (speaker reference -> microphone) using GCC-PHAT.
//!
//! The mic and the loopback reference are aligned by QPC timestamps first, so what is
//! measured here is only the stable part: output/input driver latency + acoustic flight
//! time. It is measured over ~1 s windows, confirmed several times before it is used or
//! changed, and can be warm-started from a value cached on disk.

use gamersequalizer_common::shm::AEC_CORR_POINTS;
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// 48 kHz -> 8 kHz for the correlation (speech/music echo energy is mostly below 4 kHz).
pub const DS_FACTOR: usize = 6;
pub const FULL_RATE: f32 = 48_000.0;
/// ~1.02 s of microphone per estimate.
pub const WINDOW_DS: usize = 8192;
/// Longest delay searched: 400 ms.
pub const MAX_DELAY_DS: usize = 3200;
/// Circular correlation is alias-free for lags 0..=MAX_DELAY_DS with this length.
const FFT_LEN: usize = 16384;
/// Peak-to-average ratio of the PHAT correlation below which a window is rejected.
pub const MIN_PEAK_RATIO: f32 = 10.0;
/// Peak-to-average ratio shown as full scale (100 % confidence, top of the UI curve).
pub const FULL_SCALE_PEAK_RATIO: f32 = 40.0;
/// Two estimates within 2 ms are "the same delay".
const AGREE_SAMPLES: f32 = 96.0;
/// Consecutive agreeing estimates needed for the first lock.
const CONFIRM_FIRST: u32 = 2;
/// Consecutive agreeing estimates needed to move away from a locked (or cached) delay.
const CONFIRM_SWITCH: u32 = 3;
/// Agreeing windows are averaged: cumulative mean for the first N, then an EMA with 1/N,
/// i.e. ~30 s of measurements at 1 s windows.
const REFINE_WINDOWS: u32 = 30;
/// The reported (integer) delay moves only when the average is this far from it, so it
/// doesn't flicker between neighbouring samples.
const REPORT_HYSTERESIS: f32 = 0.75;
/// Reference/mic must carry signal, otherwise the window says nothing (RMS, 8 kHz).
const MIN_REF_RMS: f32 = 0.002; // about -54 dBFS
const MIN_MIC_RMS: f32 = 0.0003; // about -70 dBFS

/// Result of one finished measurement window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowOutcome {
  /// Nothing (or too little) was playing: no information.
  ReferenceSilent,
  /// Signal present but no clear echo peak (e.g. headphones, or only near-end speech).
  NoPeak {
    peak_ratio: f32,
  },
  Estimate {
    delay_samples: f32,
    peak_ratio: f32,
  },
}

/// What happened to the locked delay after a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockChange {
  None,
  /// First lock or a confirmed jump: the echo canceller should restart its filter.
  Changed,
  /// Small refinement of the same delay.
  Refined,
}

pub struct DelayEstimator {
  fft: Arc<dyn Fft<f32>>,
  ifft: Arc<dyn Fft<f32>>,
  fft_scratch: Vec<Complex32>,
  spec_mic: Vec<Complex32>,
  spec_ref: Vec<Complex32>,
  ref_full: Vec<f32>,
  mic_ds: Vec<f32>,
  acc: f32,
  acc_n: usize,
  /// Ring index (48 kHz) of the reference frame played at the window's first mic sample.
  window_ref_idx: Option<u32>,
  corr_curve: [f32; AEC_CORR_POINTS],
  /// Long-term average of the agreeing estimates (48 kHz samples).
  locked: Option<f32>,
  /// Windows averaged into `locked` (capped at REFINE_WINDOWS).
  locked_windows: u32,
  /// What `current_delay` reports: `locked` with hysteresis.
  reported: Option<usize>,
  locked_from_cache: bool,
  candidate: Option<(f32, u32)>,
  last_outcome: Option<WindowOutcome>,
  estimate_count: u32,
}

impl Default for DelayEstimator {
  fn default() -> Self {
    Self::new()
  }
}

impl DelayEstimator {
  pub fn new() -> Self {
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_LEN);
    let ifft = planner.plan_fft_inverse(FFT_LEN);
    let scratch_len = fft.get_inplace_scratch_len().max(ifft.get_inplace_scratch_len());
    Self {
      fft,
      ifft,
      fft_scratch: vec![Complex32::default(); scratch_len],
      spec_mic: vec![Complex32::default(); FFT_LEN],
      spec_ref: vec![Complex32::default(); FFT_LEN],
      ref_full: vec![0.0; (WINDOW_DS + MAX_DELAY_DS) * DS_FACTOR],
      mic_ds: Vec::with_capacity(WINDOW_DS),
      acc: 0.0,
      acc_n: 0,
      window_ref_idx: None,
      corr_curve: [0.0; AEC_CORR_POINTS],
      locked: None,
      locked_windows: 0,
      reported: None,
      locked_from_cache: false,
      candidate: None,
      last_outcome: None,
      estimate_count: 0,
    }
  }

  /// Warm start from a delay measured in an earlier session (48 kHz samples).
  pub fn set_cached_delay(&mut self, delay_samples: usize) {
    self.set_locked(delay_samples as f32, 1);
    self.locked_from_cache = true;
    self.candidate = None;
  }

  /// Delay the echo canceller should use (48 kHz samples), once confirmed.
  pub fn current_delay(&self) -> Option<usize> {
    self.reported
  }

  pub fn is_from_cache(&self) -> bool {
    self.locked_from_cache
  }

  pub fn last_outcome(&self) -> Option<WindowOutcome> {
    self.last_outcome
  }

  pub fn estimate_count(&self) -> u32 {
    self.estimate_count
  }

  /// Correlation over delay 0..400 ms (index 0 = 0 ms) as peak-to-average ratio divided by
  /// FULL_SCALE_PEAK_RATIO (0..1): a clear echo peak stands tall, noise stays low.
  pub fn corr_curve(&self) -> &[f32; AEC_CORR_POINTS] {
    &self.corr_curve
  }

  pub fn max_delay_samples() -> usize {
    MAX_DELAY_DS * DS_FACTOR
  }

  /// Feed raw (pre-AEC) microphone samples. `ref_idx` is the ring index of the reference
  /// frame played at the time of `mic[0]`; None (no reference) restarts the window.
  /// Returns true when a full window is ready for `finish_window`.
  pub fn push_mic(&mut self, mic: &[f32], ref_idx: Option<u32>) -> bool {
    let Some(ref_idx) = ref_idx else {
      self.restart_window();
      return false;
    };
    for (i, &s) in mic.iter().enumerate() {
      if self.mic_ds.len() >= WINDOW_DS {
        break;
      }
      if self.mic_ds.is_empty() && self.acc_n == 0 {
        self.window_ref_idx = Some(ref_idx.wrapping_add(i as u32));
      }
      self.acc += s;
      self.acc_n += 1;
      if self.acc_n == DS_FACTOR {
        self.mic_ds.push(self.acc / DS_FACTOR as f32);
        self.acc = 0.0;
        self.acc_n = 0;
      }
    }
    self.mic_ds.len() >= WINDOW_DS
  }

  /// Correlate the finished window against the reference. `read_ref(start, out)` must fill
  /// `out` with mono 48 kHz reference frames from ring index `start` (silence if missing).
  pub fn finish_window(&mut self, read_ref: impl FnOnce(u32, &mut [f32])) -> (WindowOutcome, LockChange) {
    let Some(win_idx) = self.window_ref_idx else {
      self.restart_window();
      return (WindowOutcome::ReferenceSilent, LockChange::None);
    };

    // Reference span: 400 ms before the window start .. window end.
    let start = win_idx.wrapping_sub((MAX_DELAY_DS * DS_FACTOR) as u32);
    read_ref(start, &mut self.ref_full);

    let ref_len_ds = WINDOW_DS + MAX_DELAY_DS;
    let mut ref_energy = 0.0f32;
    for (j, slot) in self.spec_ref.iter_mut().enumerate() {
      let v = if j < ref_len_ds {
        let chunk = self.ref_full.get(j * DS_FACTOR..(j + 1) * DS_FACTOR).unwrap_or(&[]);
        chunk.iter().sum::<f32>() / DS_FACTOR as f32
      } else {
        0.0
      };
      ref_energy += v * v;
      *slot = Complex32::new(v, 0.0);
    }
    let mut mic_energy = 0.0f32;
    for (j, slot) in self.spec_mic.iter_mut().enumerate() {
      let v = self.mic_ds.get(j).copied().unwrap_or(0.0);
      mic_energy += v * v;
      *slot = Complex32::new(v, 0.0);
    }
    self.restart_window();

    let ref_rms = (ref_energy / ref_len_ds as f32).sqrt();
    let mic_rms = (mic_energy / WINDOW_DS as f32).sqrt();
    if ref_rms < MIN_REF_RMS || mic_rms < MIN_MIC_RMS {
      let outcome = WindowOutcome::ReferenceSilent;
      self.last_outcome = Some(outcome);
      return (outcome, LockChange::None);
    }

    // GCC-PHAT: c = IFFT( conj(MIC) * REF / |conj(MIC) * REF| )
    self.fft.process_with_scratch(&mut self.spec_mic, &mut self.fft_scratch);
    self.fft.process_with_scratch(&mut self.spec_ref, &mut self.fft_scratch);
    for (m, r) in self.spec_mic.iter_mut().zip(self.spec_ref.iter()) {
      let g = m.conj() * r;
      let mag = g.norm();
      *m = if mag > 1e-12 { g / mag } else { Complex32::default() };
    }
    self.ifft.process_with_scratch(&mut self.spec_mic, &mut self.fft_scratch);

    // c[k] = sum_i mic[i] * ref[i + k]; echo delay D (ds samples) sits at k = MAX - D.
    let corr_at = |d: usize| -> f32 { self.spec_mic.get(MAX_DELAY_DS - d.min(MAX_DELAY_DS)).map_or(0.0, |c| c.re) };
    let mut best_d = 0usize;
    let mut best = f32::MIN;
    let mut sum_abs = 0.0f32;
    for d in 0..=MAX_DELAY_DS {
      let v = corr_at(d);
      sum_abs += v.abs();
      if v > best {
        best = v;
        best_d = d;
      }
    }
    let mean_abs = sum_abs / (MAX_DELAY_DS + 1) as f32;
    let peak_ratio = if mean_abs > 1e-12 { best / mean_abs } else { 0.0 };

    // UI curve: max-pooled, in units of the peak-to-average ratio. Not normalized to the
    // window's own peak: then a window without echo (e.g. sound stopped mid-window) drew its
    // highest noise spike at full height, looking like a confident measurement.
    let scale = if mean_abs > 1e-12 { 1.0 / (mean_abs * FULL_SCALE_PEAK_RATIO) } else { 0.0 };
    let bin = (MAX_DELAY_DS + 1).div_ceil(AEC_CORR_POINTS);
    for (b, slot) in self.corr_curve.iter_mut().enumerate() {
      let mut m = 0.0f32;
      for d in b * bin..((b + 1) * bin).min(MAX_DELAY_DS + 1) {
        m = m.max(corr_at(d).max(0.0));
      }
      *slot = (m * scale).min(1.0);
    }

    if peak_ratio < MIN_PEAK_RATIO {
      let outcome = WindowOutcome::NoPeak { peak_ratio };
      self.last_outcome = Some(outcome);
      return (outcome, LockChange::None);
    }

    // Parabolic interpolation around the peak for sub-sample resolution.
    let y0 = if best_d > 0 { corr_at(best_d - 1) } else { best };
    let y2 = if best_d < MAX_DELAY_DS { corr_at(best_d + 1) } else { best };
    let denom = y0 - 2.0 * best + y2;
    let frac = if denom.abs() > 1e-12 { (0.5 * (y0 - y2) / denom).clamp(-0.5, 0.5) } else { 0.0 };
    let delay_samples = ((best_d as f32 + frac) * DS_FACTOR as f32).max(0.0);

    self.estimate_count = self.estimate_count.wrapping_add(1);
    let outcome = WindowOutcome::Estimate { delay_samples, peak_ratio };
    self.last_outcome = Some(outcome);
    (outcome, self.update_lock(delay_samples))
  }

  fn update_lock(&mut self, est: f32) -> LockChange {
    if let Some(cur) = self.locked {
      if (est - cur).abs() <= AGREE_SAMPLES {
        let n = (self.locked_windows + 1).min(REFINE_WINDOWS);
        self.set_locked(cur + (est - cur) / n as f32, n);
        self.locked_from_cache = false;
        self.candidate = None;
        return LockChange::Refined;
      }
    }

    let needed = if self.locked.is_some() { CONFIRM_SWITCH } else { CONFIRM_FIRST };
    let count = match self.candidate {
      Some((c, n)) if (est - c).abs() <= AGREE_SAMPLES => n + 1,
      _ => 1,
    };
    self.candidate = Some((est, count));
    if count >= needed {
      self.set_locked(est, 1);
      self.reported = Some(est.round().max(0.0) as usize);
      self.locked_from_cache = false;
      self.candidate = None;
      return LockChange::Changed;
    }
    LockChange::None
  }

  fn set_locked(&mut self, average: f32, windows: u32) {
    self.locked = Some(average);
    self.locked_windows = windows;
    let moved = self.reported.is_none_or(|r| (average - r as f32).abs() >= REPORT_HYSTERESIS);
    if moved {
      self.reported = Some(average.round().max(0.0) as usize);
    }
  }

  fn restart_window(&mut self) {
    self.mic_ds.clear();
    self.acc = 0.0;
    self.acc_n = 0;
    self.window_ref_idx = None;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const BLOCK: usize = 480;

  /// Deterministic white-ish noise (LCG), so tests don't need a rand dependency.
  fn noise(len: usize, seed: u32) -> Vec<f32> {
    let mut x = seed;
    (0..len)
      .map(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.5
      })
      .collect()
  }

  /// Feeds `seconds` of mic = 0.3 * ref delayed by `delay` (+ near-end noise).
  fn run(
    est: &mut DelayEstimator,
    delay: usize,
    seconds: usize,
    reference: &[f32],
  ) -> Vec<(WindowOutcome, LockChange)> {
    let total = seconds * 48_000;
    let near = noise(total, 7);
    let mut results = Vec::new();
    let offset = MAX_DELAY_DS * DS_FACTOR + 1000; // ring index of mic sample 0
    let mut n = 0;
    while n + BLOCK <= total {
      let mic: Vec<f32> = (0..BLOCK)
        .map(|i| {
          let t = offset + n + i;
          0.3 * reference.get(t - delay).copied().unwrap_or(0.0) + 0.02 * near[n + i]
        })
        .collect();
      if est.push_mic(&mic, Some((offset + n) as u32)) {
        results.push(est.finish_window(|start, out| {
          for (k, o) in out.iter_mut().enumerate() {
            *o = reference.get(start as usize + k).copied().unwrap_or(0.0);
          }
        }));
      }
      n += BLOCK;
    }
    results
  }

  #[test]
  fn test_measures_known_delay_and_locks() {
    let reference = noise(12 * 48_000, 1);
    let mut est = DelayEstimator::new();
    let results = run(&mut est, 2_345, 5, &reference);
    assert!(results.iter().any(|(_, c)| *c == LockChange::Changed));
    let d = est.current_delay().unwrap_or(0);
    assert!((d as i64 - 2_345).abs() <= 6, "expected ~2345 samples, got {d}");
  }

  #[test]
  fn test_window_without_echo_draws_a_low_curve() {
    // Reference and mic both carry signal but are unrelated: no peak, and the UI curve
    // must stay below the acceptance threshold instead of spiking to full scale.
    let reference = noise((WINDOW_DS + MAX_DELAY_DS) * DS_FACTOR + 4096, 5);
    let mic = noise(WINDOW_DS * DS_FACTOR, 3);
    let mut est = DelayEstimator::new();
    assert!(est.push_mic(&mic, Some((MAX_DELAY_DS * DS_FACTOR) as u32)));
    let (outcome, _) = est.finish_window(|start, out| {
      for (k, o) in out.iter_mut().enumerate() {
        *o = reference.get(start as usize + k).copied().unwrap_or(0.0);
      }
    });
    assert!(matches!(outcome, WindowOutcome::NoPeak { .. }), "{outcome:?}");
    let top = est.corr_curve().iter().fold(0.0f32, |m, v| m.max(*v));
    assert!(top < MIN_PEAK_RATIO / FULL_SCALE_PEAK_RATIO, "curve top {top}");
  }

  #[test]
  fn test_silent_reference_never_locks() {
    let reference = vec![0.0f32; 12 * 48_000];
    let mut est = DelayEstimator::new();
    let results = run(&mut est, 1_000, 4, &reference);
    assert!(results.iter().all(|(o, _)| *o == WindowOutcome::ReferenceSilent));
    assert_eq!(est.current_delay(), None);
  }

  #[test]
  fn test_cached_delay_needs_confirmations_to_move() {
    let reference = noise(12 * 48_000, 3);
    let mut est = DelayEstimator::new();
    est.set_cached_delay(500);
    assert_eq!(est.current_delay(), Some(500));

    // Two windows at a different delay are not enough to move a cached lock...
    let _ = run(&mut est, 4_000, 2, &reference);
    assert_eq!(est.current_delay(), Some(500));
    // ...but a sustained change is.
    let _ = run(&mut est, 4_000, 4, &reference);
    let d = est.current_delay().unwrap_or(0);
    assert!((d as i64 - 4_000).abs() <= 6, "expected switch to ~4000, got {d}");
  }

  #[test]
  fn refined_delay_is_averaged_and_does_not_flicker() {
    let mut est = DelayEstimator::new();
    est.set_cached_delay(45);
    let mut reported = std::collections::BTreeSet::new();
    for i in 0..60 {
      // Window estimates jitter by +-0.6 samples around 45.
      let jitter = if i % 2 == 0 { 0.6 } else { -0.6 };
      assert_eq!(est.update_lock(45.0 + jitter), LockChange::Refined);
      reported.insert(est.current_delay());
    }
    assert_eq!(reported.into_iter().collect::<Vec<_>>(), vec![Some(45)]);
    // A real shift still moves it: gradually (~30 s time constant), within the agreement band.
    for _ in 0..150 {
      est.update_lock(48.0);
    }
    assert_eq!(est.current_delay(), Some(48));
  }
}
