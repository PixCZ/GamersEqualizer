//! Acoustic echo canceller: multidelay block frequency-domain adaptive filter (MDF, the
//! structure Speex uses) with the learning rate set per frequency from an estimate of the
//! residual echo (J.-M. Valin, "On adjusting the learning rate in frequency domain echo
//! cancellation with double-talk", 2007). It adapts fast while the error is mostly echo and
//! almost stops while it is mostly near-end voice or noise, so it needs no double-talk detector.
//!
//! Two filters: the background one adapts every frame; the foreground one produces the output
//! and takes the background weights only when they leave clearly less error. The background is
//! put back to the foreground when it does clearly worse (it adapted on voice). A last guard
//! resets both when the output gets louder than the microphone: an echo canceller may fail to
//! remove echo, it must never add noise (the NLMS before this did, up to 15 dB, debug capture
//! 2026-11-07).
//!
//! The bulk delay is removed beforehand (timestamps + `DelayEstimator`), so the filter only
//! models the room response around it. Frames of 5 ms (240 samples, FFT 480): blocks that are
//! a multiple of 240 pass with no added latency; any other block size switches once to a 5 ms
//! buffer.

use gamersequalizer_common::shm::AEC_IMPULSE_POINTS;
use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::collections::VecDeque;
use std::sync::Arc;

/// Samples per frame (5 ms at 48 kHz).
pub const FRAME: usize = 240;
const FFT_LEN: usize = 2 * FRAME;
const BINS: usize = FRAME + 1;
/// 5 partitions x 5 ms = 25 ms: from `PRE_DELAY_SAMPLES` before the measured delay to ~22 ms
/// after it (direct path + early reflections of a desk setup).
pub const DEFAULT_FILTER_TAPS: usize = 5 * FRAME;
/// Filter starts this far before the measured delay so estimation error can't cut the direct
/// path off. Shows up as the impulse peak sitting at ~3 ms in the UI.
pub const PRE_DELAY_SAMPLES: usize = 144;
/// Pre-emphasis on reference and mic (inverted on the output): flattens the spectrum of
/// speech/music so the bass doesn't dominate the adaptation.
const PREEMPH: f32 = 0.9;
/// Reference mean square under which nothing adapts (-80 dBFS).
const MIN_REF_POWER: f32 = 1e-8;
/// Regularization of the per-bin normalization, as a mean-square level (-70 dBFS).
const REG_POWER: f32 = 1e-7;
/// Bounds of the echo leakage estimate (residual echo / echo estimate, power).
const MIN_LEAK: f32 = 0.005;
/// Upper bound of the residual-to-error ratio (caps the learning rate at 0.5).
const MAX_RER: f32 = 0.5;
/// Fixed learning rate until the leakage estimate is trustworthy.
const STARTUP_RATE: f32 = 0.25;
/// Leakage smoothing: rate per frame relative to echo/error energy, and its cap.
const LEAK_BETA0: f32 = 2.0 * FRAME as f32 / 48_000.0;
const LEAK_BETA_MAX: f32 = 0.5 * FRAME as f32 / 48_000.0;
// Foreground/background decision: variance tests on the error difference between the filters.
const VAR1_SMOOTH: f32 = 0.36;
const VAR2_SMOOTH: f32 = 0.7225;
const VAR1_UPDATE: f32 = 0.5;
const VAR2_UPDATE: f32 = 0.25;
const VAR_BACKTRACK: f32 = 4.0;
/// Smoothed output power above the mic's by this factor (3 dB): both filters are reset.
const HARM_RATIO: f32 = 2.0;
/// Residual-to-error ratio under which the error is mostly near-end (voice or noise): the
/// filter barely adapts. Reported to the UI as "double talk".
const NEAR_END_RER: f32 = 0.05;
/// Frames between refreshes of the impulse response shown in the UI (100 ms).
const IMPULSE_EVERY: u32 = 20;

pub struct AcousticEchoCanceller {
  parts: usize,
  r2c: Arc<dyn RealToComplex<f32>>,
  c2r: Arc<dyn ComplexToReal<f32>>,
  scratch_fwd: Vec<Complex32>,
  scratch_inv: Vec<Complex32>,
  time: Vec<f32>,
  spec: Vec<Complex32>,
  /// Reference spectra of the last `parts` frames (each over two frames), newest at `x_head`.
  x_spec: Vec<Complex32>,
  x_head: usize,
  x_prev: Vec<f32>,
  /// Per bin: sum of |X|^2 over all partitions (the NLMS normalization).
  x_power: Vec<f32>,
  /// Background (adapting) and foreground (output) filters, `parts` x `BINS`.
  bg: Vec<Complex32>,
  fg: Vec<Complex32>,
  err_spec: Vec<Complex32>,
  /// This frame's error / echo estimate power spectra, and their slow averages.
  err_power: Vec<f32>,
  echo_power: Vec<f32>,
  err_power_avg: Vec<f32>,
  echo_power_avg: Vec<f32>,
  step: Vec<f32>,
  /// Leakage estimate: correlation of error and echo power fluctuations.
  pey: f32,
  pyy: f32,
  leak: f32,
  sum_adapt: f32,
  adapted: bool,
  davg1: f32,
  davg2: f32,
  dvar1: f32,
  dvar2: f32,
  /// Partition whose time-domain constraint is applied next (one per frame, as Speex).
  constrain_next: usize,
  mem_x: f32,
  mem_d: f32,
  mem_e: f32,
  x_frame: Vec<f32>,
  d_frame: Vec<f32>,
  y_bg: Vec<f32>,
  y_fg: Vec<f32>,
  e_bg: Vec<f32>,
  e_fg: Vec<f32>,
  // Block <-> frame buffering.
  in_ref: Vec<f32>,
  in_mic: Vec<f32>,
  in_len: usize,
  out_queue: VecDeque<f32>,
  buffered: bool,
  // "Never louder than the mic" guard (pre-emphasized powers).
  mic_smooth: f32,
  out_smooth: f32,
  // Telemetry.
  erle_db: f32,
  erle_mic: f32,
  erle_out: f32,
  near_end: bool,
  frames: u32,
  impulse: [f32; AEC_IMPULSE_POINTS],
  impulse_time: Vec<f32>,
}

impl AcousticEchoCanceller {
  /// `taps` is rounded up to whole 5 ms partitions (1..16).
  pub fn new(taps: usize) -> Self {
    let parts = taps.div_ceil(FRAME).clamp(1, 16);
    let mut planner = RealFftPlanner::<f32>::new();
    let r2c = planner.plan_fft_forward(FFT_LEN);
    let c2r = planner.plan_fft_inverse(FFT_LEN);
    let zero = Complex32::new(0.0, 0.0);
    Self {
      parts,
      scratch_fwd: r2c.make_scratch_vec(),
      scratch_inv: c2r.make_scratch_vec(),
      r2c,
      c2r,
      time: vec![0.0; FFT_LEN],
      spec: vec![zero; BINS],
      x_spec: vec![zero; parts * BINS],
      x_head: 0,
      x_prev: vec![0.0; FRAME],
      x_power: vec![0.0; BINS],
      bg: vec![zero; parts * BINS],
      fg: vec![zero; parts * BINS],
      err_spec: vec![zero; BINS],
      err_power: vec![0.0; BINS],
      echo_power: vec![0.0; BINS],
      err_power_avg: vec![0.0; BINS],
      echo_power_avg: vec![0.0; BINS],
      step: vec![0.0; BINS],
      pey: 0.0,
      pyy: 0.0,
      leak: MIN_LEAK,
      sum_adapt: 0.0,
      adapted: false,
      davg1: 0.0,
      davg2: 0.0,
      dvar1: 0.0,
      dvar2: 0.0,
      constrain_next: 0,
      mem_x: 0.0,
      mem_d: 0.0,
      mem_e: 0.0,
      x_frame: vec![0.0; FRAME],
      d_frame: vec![0.0; FRAME],
      y_bg: vec![0.0; FRAME],
      y_fg: vec![0.0; FRAME],
      e_bg: vec![0.0; FRAME],
      e_fg: vec![0.0; FRAME],
      in_ref: vec![0.0; FRAME],
      in_mic: vec![0.0; FRAME],
      in_len: 0,
      out_queue: VecDeque::with_capacity(8192),
      buffered: false,
      mic_smooth: 0.0,
      out_smooth: 0.0,
      erle_db: 0.0,
      erle_mic: 0.0,
      erle_out: 0.0,
      near_end: false,
      frames: 0,
      impulse: [0.0; AEC_IMPULSE_POINTS],
      impulse_time: vec![0.0; parts * FRAME],
    }
  }

  pub fn default_48k() -> Self {
    Self::new(DEFAULT_FILTER_TAPS)
  }

  pub fn taps(&self) -> usize {
    self.parts * FRAME
  }

  /// Process a block in place. `ref_block` must be delay-aligned to `mic_block`.
  pub fn process_block(&mut self, ref_block: &[f32], mic_block: &mut [f32]) {
    let n = mic_block.len().min(ref_block.len());
    if !self.buffered && (self.in_len != 0 || !n.is_multiple_of(FRAME)) {
      // This host's blocks don't line up with frames: 5 ms of latency from now on (once).
      self.buffered = true;
      self.out_queue.extend(std::iter::repeat_n(0.0, FRAME));
    }
    for (&m, &r) in mic_block.iter().zip(ref_block) {
      if let (Some(rs), Some(ms)) = (self.in_ref.get_mut(self.in_len), self.in_mic.get_mut(self.in_len)) {
        (*rs, *ms) = (r, m);
      }
      self.in_len += 1;
      if self.in_len == FRAME {
        self.in_len = 0;
        self.process_frame();
      }
    }
    for s in mic_block.iter_mut().take(n) {
      *s = self.out_queue.pop_front().unwrap_or(0.0);
    }
  }

  pub fn erle_db(&self) -> f32 {
    self.erle_db
  }

  /// The error is mostly near-end (voice or noise) while audio plays: the filter barely adapts.
  pub fn is_double_talk(&self) -> bool {
    self.near_end
  }

  /// Max |w| per group of taps (output filter), oldest-delay first: the echo path impulse response.
  pub fn impulse_bins(&self, out: &mut [f32; AEC_IMPULSE_POINTS]) {
    *out = self.impulse;
  }

  pub fn reset(&mut self) {
    let zero = Complex32::new(0.0, 0.0);
    for v in [&mut self.x_spec, &mut self.bg, &mut self.fg, &mut self.err_spec] {
      v.fill(zero);
    }
    for v in [
      &mut self.x_prev,
      &mut self.x_power,
      &mut self.err_power_avg,
      &mut self.echo_power_avg,
      &mut self.in_ref,
      &mut self.in_mic,
      &mut self.impulse_time,
    ] {
      v.fill(0.0);
    }
    self.restart_adaptation();
    (self.x_head, self.constrain_next, self.in_len, self.frames) = (0, 0, 0, 0);
    (self.mem_x, self.mem_d, self.mem_e) = (0.0, 0.0, 0.0);
    self.out_queue.clear();
    self.buffered = false;
    (self.erle_db, self.erle_mic, self.erle_out, self.near_end) = (0.0, 0.0, 0.0, false);
    self.impulse = [0.0; AEC_IMPULSE_POINTS];
  }

  /// Forget both filters and the learning state; keeps the signal history (no glitch).
  fn restart_adaptation(&mut self) {
    let zero = Complex32::new(0.0, 0.0);
    self.bg.fill(zero);
    self.fg.fill(zero);
    (self.pey, self.pyy, self.leak, self.sum_adapt, self.adapted) = (0.0, 0.0, MIN_LEAK, 0.0, false);
    (self.davg1, self.davg2, self.dvar1, self.dvar2) = (0.0, 0.0, 0.0, 0.0);
    (self.mic_smooth, self.out_smooth) = (0.0, 0.0);
  }

  fn process_frame(&mut self) {
    let parts = self.parts;

    // Pre-emphasis.
    for ((xo, &x), (dout, &d)) in self.x_frame.iter_mut().zip(&self.in_ref).zip(self.d_frame.iter_mut().zip(&self.in_mic)) {
      *xo = x - PREEMPH * self.mem_x;
      self.mem_x = x;
      *dout = d - PREEMPH * self.mem_d;
      self.mem_d = d;
    }

    // Spectrum of [previous frame, this frame] becomes the newest partition.
    self.x_head = (self.x_head + parts - 1) % parts;
    self.time[..FRAME].copy_from_slice(&self.x_prev);
    self.time[FRAME..].copy_from_slice(&self.x_frame);
    self.x_prev.copy_from_slice(&self.x_frame);
    let head = self.x_head * BINS;
    fft(&*self.r2c, &mut self.time, &mut self.x_spec[head..head + BINS], &mut self.scratch_fwd);
    self.x_power.fill(0.0);
    for part in self.x_spec.as_chunks::<BINS>().0 {
      for (p, x) in self.x_power.iter_mut().zip(part) {
        *p += x.norm_sqr();
      }
    }

    // Echo estimates of both filters (with the weights from before this frame's update).
    let mut filt = Filtering { x_spec: &self.x_spec, x_head: self.x_head, parts, c2r: &*self.c2r };
    filt.output(&self.bg, &mut self.spec, &mut self.time, &mut self.scratch_inv, &mut self.y_bg);
    filt.output(&self.fg, &mut self.spec, &mut self.time, &mut self.scratch_inv, &mut self.y_fg);

    let (mut see, mut sff, mut sdd, mut syy, mut sey, mut dbf) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for i in 0..FRAME {
      let (d, yb, yf) = (self.d_frame[i], self.y_bg[i], self.y_fg[i]);
      let (eb, ef) = (d - yb, d - yf);
      self.e_bg[i] = eb;
      self.e_fg[i] = ef;
      see += eb * eb;
      sff += ef * ef;
      sdd += d * d;
      syy += yb * yb;
      sey += eb * yb;
      dbf += (yb - yf) * (yb - yf);
    }
    let sxx: f32 = self.x_frame.iter().map(|x| x * x).sum();
    let far_end = sxx > FRAME as f32 * MIN_REF_POWER;
    let tiny = FRAME as f32 * 1e-14;

    // Error and echo-estimate spectra (second half windowed, as the gradient needs).
    self.time[..FRAME].fill(0.0);
    self.time[FRAME..].copy_from_slice(&self.e_bg);
    fft(&*self.r2c, &mut self.time, &mut self.err_spec, &mut self.scratch_fwd);
    self.time[..FRAME].fill(0.0);
    self.time[FRAME..].copy_from_slice(&self.y_bg);
    fft(&*self.r2c, &mut self.time, &mut self.spec, &mut self.scratch_fwd);
    for k in 0..BINS {
      self.err_power[k] = self.err_spec[k].norm_sqr();
      self.echo_power[k] = self.spec[k].norm_sqr();
    }

    // Leakage: how much of the echo estimate's power fluctuation shows up in the error.
    let ss = 0.35 / parts as f32;
    let (mut pey_frame, mut pyy_frame) = (0.0f32, 0.0f32);
    for k in 0..BINS {
      let de = self.err_power[k] - self.err_power_avg[k];
      let dy = self.echo_power[k] - self.echo_power_avg[k];
      pey_frame += de * dy;
      pyy_frame += dy * dy;
      self.err_power_avg[k] += ss * (self.err_power[k] - self.err_power_avg[k]);
      self.echo_power_avg[k] += ss * (self.echo_power[k] - self.echo_power_avg[k]);
    }
    if far_end && see > tiny {
      let alpha = (LEAK_BETA0 * syy / see).min(LEAK_BETA_MAX);
      self.pey += alpha * (pey_frame - self.pey);
      self.pyy += alpha * (pyy_frame - self.pyy);
    }
    let pyy = self.pyy.max(1e-30);
    self.leak = (self.pey.clamp(MIN_LEAK * pyy, pyy) / pyy).clamp(MIN_LEAK, 1.0);

    // Residual echo as a share of the error.
    let mut rer = (1e-4 * sxx + 3.0 * self.leak * syy) / (see + tiny);
    rer = rer.max(sey * sey / (see * syy + tiny * tiny)).min(MAX_RER);

    // Foreground / background.
    let diff = sff - see;
    let mut take_bg = diff * diff.abs() > sff * dbf;
    self.davg1 = 0.6 * self.davg1 + 0.4 * diff;
    self.dvar1 = VAR1_SMOOTH * self.dvar1 + 0.16 * sff * dbf;
    self.davg2 = 0.85 * self.davg2 + 0.15 * diff;
    self.dvar2 = VAR2_SMOOTH * self.dvar2 + 0.0225 * sff * dbf;
    take_bg |= self.davg1 * self.davg1.abs() > VAR1_UPDATE * self.dvar1;
    take_bg |= self.davg2 * self.davg2.abs() > VAR2_UPDATE * self.dvar2;
    let mut restored = false;
    if take_bg {
      self.fg.copy_from_slice(&self.bg);
      (self.davg1, self.davg2, self.dvar1, self.dvar2) = (0.0, 0.0, 0.0, 0.0);
      // Crossfade the output within this frame to the filter that takes over.
      for (i, (ef, &eb)) in self.e_fg.iter_mut().zip(&self.e_bg).enumerate() {
        *ef += (eb - *ef) * (i + 1) as f32 / FRAME as f32;
      }
    } else if -diff * diff.abs() > VAR_BACKTRACK * sff * dbf {
      self.bg.copy_from_slice(&self.fg);
      (self.davg1, self.davg2, self.dvar1, self.dvar2) = (0.0, 0.0, 0.0, 0.0);
      restored = true;
    }

    // Adapt the background filter.
    if far_end && !restored {
      let reg = parts as f32 * FFT_LEN as f32 * REG_POWER;
      if self.adapted {
        for k in 0..BINS {
          let e = self.err_power[k] + tiny;
          // Valin's per-bin rate only. Speex adds 0.3 x the frame-wide residual ratio to every
          // bin; that floor let a low voice tone (small after pre-emphasis) drive adaptation.
          let r = (self.leak * self.echo_power[k]).min(0.5 * e);
          self.step[k] = r / (e * (self.x_power[k] + reg));
        }
      } else {
        let rate = STARTUP_RATE * (sxx / (see + tiny)).min(1.0);
        self.sum_adapt += rate;
        for (s, p) in self.step.iter_mut().zip(&self.x_power) {
          *s = rate / (p + reg);
        }
        self.adapted = self.sum_adapt > parts as f32 && self.leak > 0.03;
      }
      for m in 0..parts {
        let xi = (self.x_head + m) % parts * BINS;
        let (w, x) = (&mut self.bg[m * BINS..(m + 1) * BINS], &self.x_spec[xi..xi + BINS]);
        for k in 0..BINS {
          w[k] += x[k].conj() * self.err_spec[k] * self.step[k];
        }
      }
      // Gradient constraint (linear, not circular, convolution) on one partition per frame.
      let c = self.constrain_next * BINS;
      self.constrain_next = (self.constrain_next + 1) % parts;
      self.spec.copy_from_slice(&self.bg[c..c + BINS]);
      ifft(&*self.c2r, &mut self.spec, &mut self.time, &mut self.scratch_inv);
      for v in &mut self.time[..FRAME] {
        *v /= FFT_LEN as f32;
      }
      self.time[FRAME..].fill(0.0);
      fft(&*self.r2c, &mut self.time, &mut self.bg[c..c + BINS], &mut self.scratch_fwd);
    }

    // Never louder than the mic: a filter that adds noise is worse than none.
    let sout: f32 = self.e_fg.iter().map(|e| e * e).sum();
    self.mic_smooth = 0.9 * self.mic_smooth + 0.1 * sdd;
    self.out_smooth = 0.9 * self.out_smooth + 0.1 * sout;
    let broken = !(see.is_finite() && sff.is_finite() && syy.is_finite());
    if broken || (self.out_smooth > HARM_RATIO * self.mic_smooth && self.mic_smooth > tiny) {
      self.restart_adaptation();
      self.e_fg.copy_from_slice(&self.d_frame);
    }

    // De-emphasis and output.
    for &e in &self.e_fg {
      self.mem_e = e + PREEMPH * self.mem_e;
      self.out_queue.push_back(self.mem_e);
    }

    // Telemetry.
    self.near_end = far_end && self.adapted && rer < NEAR_END_RER;
    if far_end && !self.near_end {
      self.erle_mic = 0.9 * self.erle_mic + 0.1 * sdd;
      self.erle_out = 0.9 * self.erle_out + 0.1 * sout.min(sdd);
      if self.erle_mic > tiny && self.erle_out > 0.0 {
        let db = (10.0 * (self.erle_mic / self.erle_out).log10()).clamp(0.0, 60.0);
        self.erle_db = 0.98 * self.erle_db + 0.02 * db;
      }
    }
    self.frames = self.frames.wrapping_add(1);
    if self.frames.is_multiple_of(IMPULSE_EVERY) {
      self.refresh_impulse();
    }
  }

  fn refresh_impulse(&mut self) {
    for (part, taps) in self.fg.as_chunks::<BINS>().0.iter().zip(self.impulse_time.as_chunks_mut::<FRAME>().0) {
      self.spec.copy_from_slice(part);
      ifft(&*self.c2r, &mut self.spec, &mut self.time, &mut self.scratch_inv);
      for (t, &v) in taps.iter_mut().zip(&self.time) {
        *t = v / FFT_LEN as f32;
      }
    }
    let group = (self.impulse_time.len() / AEC_IMPULSE_POINTS).max(1);
    for (slot, g) in self.impulse.iter_mut().zip(self.impulse_time.chunks(group)) {
      *slot = g.iter().fold(0.0f32, |m, w| m.max(w.abs()));
    }
  }
}

/// Filter output y = last frame of IFFT(sum_m W_m X_m).
struct Filtering<'a> {
  x_spec: &'a [Complex32],
  x_head: usize,
  parts: usize,
  c2r: &'a dyn ComplexToReal<f32>,
}

impl Filtering<'_> {
  fn output(&mut self, w: &[Complex32], spec: &mut [Complex32], time: &mut [f32], scratch: &mut [Complex32], y: &mut [f32]) {
    spec.fill(Complex32::new(0.0, 0.0));
    for (m, wm) in w.as_chunks::<BINS>().0.iter().enumerate().take(self.parts) {
      let xi = (self.x_head + m) % self.parts * BINS;
      for ((s, &wk), &xk) in spec.iter_mut().zip(wm).zip(&self.x_spec[xi..xi + BINS]) {
        *s += wk * xk;
      }
    }
    ifft(self.c2r, spec, time, scratch);
    for (o, &t) in y.iter_mut().zip(&time[FRAME..]) {
      *o = t / FFT_LEN as f32;
    }
  }
}

/// Forward real FFT (unnormalized); `time` is used as scratch. On error the spectrum is zero.
fn fft(r2c: &dyn RealToComplex<f32>, time: &mut [f32], out: &mut [Complex32], scratch: &mut [Complex32]) {
  if r2c.process_with_scratch(time, out, scratch).is_err() {
    out.fill(Complex32::new(0.0, 0.0));
  }
}

/// Inverse real FFT (unnormalized, x FFT_LEN); `spec` is used as scratch. On error the output is zero.
fn ifft(c2r: &dyn ComplexToReal<f32>, spec: &mut [Complex32], out: &mut [f32], scratch: &mut [Complex32]) {
  // A real signal's DC and Nyquist bins are real; float noise would make realfft reject them.
  if let Some(dc) = spec.first_mut() {
    dc.im = 0.0;
  }
  if let Some(nyq) = spec.last_mut() {
    nyq.im = 0.0;
  }
  if c2r.process_with_scratch(spec, out, scratch).is_err() {
    out.fill(0.0);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Regression (debug capture 2026-11-07): speech-like reference, echo at room-noise level.
  /// The NLMS before the MDF made the output louder than the raw mic (4 dB here, 15 dB live).
  #[test]
  fn test_never_louder_than_the_mic_with_weak_echo_under_speech() {
    let mut aec = AcousticEchoCanceller::default_48k();
    let len = 8 * 48_000;
    let speaker = speech_like(len, 3);
    let room = noise(len, 9);
    let mic: Vec<f32> = (0..len).map(|n| delayed(&speaker, n, 46) * 0.01 + room[n] * 0.004).collect();

    let (out, _) = run(&mut aec, &speaker, &mic, 480);
    let gain_db = energy_db(&out[4 * 48_000..]) - energy_db(&mic[4 * 48_000..]);
    assert!(gain_db < 0.5, "AEC made the mic {gain_db:.1} dB louder");
  }

  #[test]
  fn test_cancels_delayed_echo_with_room_response() {
    let mut aec = AcousticEchoCanceller::new(256);
    let speaker = noise(4 * 48_000, 11);
    // Echo path: direct path 40 samples late + a reflection at 120 samples.
    let echo: Vec<f32> = (0..speaker.len()).map(|n| delayed(&speaker, n, 40) * 0.4 - delayed(&speaker, n, 120) * 0.15).collect();

    let (out, _) = run(&mut aec, &speaker, &echo, 480);
    let attenuation_db = energy_db(&echo[2 * 48_000..]) - energy_db(&out[2 * 48_000..]);
    assert!(attenuation_db > 20.0, "expected > 20 dB echo removal, got {attenuation_db:.1}");
    assert!(aec.erle_db() > 10.0, "ERLE {:.1}", aec.erle_db());

    let mut ir = [0.0f32; AEC_IMPULSE_POINTS];
    aec.impulse_bins(&mut ir);
    let peak_bin = ir.iter().enumerate().fold((0, 0.0), |b, (i, &v)| if v > b.1 { (i, v) } else { b }).0;
    assert_eq!(peak_bin, 40 / (aec.taps() / AEC_IMPULSE_POINTS), "impulse peak must sit at the direct path");
  }

  /// Loud near-end voice over the far end: it passes untouched and the filter keeps the echo path.
  #[test]
  fn test_double_talk_keeps_voice_and_echo_path() {
    let mut aec = AcousticEchoCanceller::new(256);
    let len = 6 * 48_000;
    let speaker = noise(len, 5);
    let voice_at = 3 * 48_000;
    let voice: Vec<f32> = (0..len).map(|n| if n >= voice_at { 0.5 * (n as f32 * 0.0393).sin() } else { 0.0 }).collect();
    let echo: Vec<f32> = (0..len).map(|n| delayed(&speaker, n, 20) * 0.3).collect();
    let mic: Vec<f32> = echo.iter().zip(&voice).map(|(e, v)| e + v).collect();

    let (out, near_end) = run(&mut aec, &speaker, &mic, 480);
    assert!(near_end.iter().skip(voice_at / 480 + 20).any(|&d| d), "voice over the far end must read as double talk");
    let tail = voice_at + 48_000..len;
    let residual: Vec<f32> = out[tail.clone()].iter().zip(&voice[tail.clone()]).map(|(o, v)| o - v).collect();
    let echo_removed_db = energy_db(&echo[tail.clone()]) - energy_db(&residual);
    assert!(echo_removed_db > 15.0, "echo must stay cancelled during double talk, got {echo_removed_db:.1} dB");
    let voice_change_db = energy_db(&out[tail.clone()]) - energy_db(&voice[tail]);
    assert!(voice_change_db.abs() < 1.0, "voice level changed by {voice_change_db:.1} dB");
  }

  /// Voice quieter than the loud reference (a level detector would miss it): the echo stays cancelled.
  #[test]
  fn test_quiet_voice_under_loud_music_does_not_leak_echo() {
    let mut aec = AcousticEchoCanceller::new(256);
    let len = 6 * 48_000;
    let speaker = noise(len, 7);
    let echo: Vec<f32> = (0..len).map(|n| delayed(&speaker, n, 20) * 0.3).collect();
    let voice_at = 3 * 48_000;
    let voice: Vec<f32> =
      (0..len).map(|n| if n >= voice_at { 0.2 * (n as f32 * 2.0 * std::f32::consts::PI * 300.0 / 48_000.0).sin() } else { 0.0 }).collect();
    let mic: Vec<f32> = echo.iter().zip(&voice).map(|(e, v)| e + v).collect();

    let (out, _) = run(&mut aec, &speaker, &mic, 480);
    let tail = voice_at + 12_000..len;
    let residual: Vec<f32> = out[tail.clone()].iter().zip(&voice[tail.clone()]).map(|(o, v)| o - v).collect();
    let attenuation_db = energy_db(&echo[tail]) - energy_db(&residual);
    assert!(attenuation_db > 20.0, "echo leaked during double talk: only {attenuation_db:.1} dB removed");
  }

  /// The echo path changes (mic moved, volume changed): cancelled again within ~1 s.
  #[test]
  fn test_relearns_after_echo_path_change() {
    let mut aec = AcousticEchoCanceller::default_48k();
    let len = 5 * 48_000;
    let change_at = 2 * 48_000;
    let speaker = speech_like(len, 21);
    let room = noise(len, 4);
    let echo: Vec<f32> = (0..len)
      .map(|n| if n < change_at { delayed(&speaker, n, 20) * 0.3 } else { delayed(&speaker, n, 300) * -0.15 + delayed(&speaker, n, 80) * 0.1 })
      .collect();
    let mic: Vec<f32> = echo.iter().zip(&room).map(|(e, r)| e + r * 0.0005).collect();

    let (out, _) = run(&mut aec, &speaker, &mic, 480);
    let before = energy_db(&echo[change_at - 48_000..change_at]) - energy_db(&out[change_at - 48_000..change_at]);
    let after = energy_db(&echo[change_at + 48_000..]) - energy_db(&out[change_at + 48_000..]);
    assert!(before > 20.0, "converged before the change: {before:.1} dB");
    assert!(after > 15.0, "relearned after the change: only {after:.1} dB");
  }

  /// Blocks that are whole frames pass with no latency; others get exactly one frame.
  #[test]
  fn test_latency_zero_for_whole_frames_one_frame_otherwise() {
    let mic = noise(48_000, 13);
    let silent = vec![0.0f32; mic.len()];
    let (out, _) = run(&mut AcousticEchoCanceller::default_48k(), &silent, &mic, 480);
    assert!(out.iter().zip(&mic).all(|(o, m)| (o - m).abs() < 1e-4), "480-sample blocks must pass unchanged");

    let (out, _) = run(&mut AcousticEchoCanceller::default_48k(), &silent, &mic, 441);
    assert!(out.iter().take(FRAME).all(|&o| o == 0.0));
    assert!(out.iter().skip(FRAME).zip(&mic).all(|(o, m)| (o - m).abs() < 1e-4), "441-sample blocks: one frame late");
  }

  /// Replays a debug capture (`AEC_CAPTURE=<capture dir>`): per 5 s raw mic, what the live chain
  /// produced and what this build produces (dBFS); then the real reference through a known
  /// path (46 samples, -40 dB) + the capture's room noise, where the result must not exceed the input.
  ///   $env:AEC_CAPTURE='test_data/private/captures/<time>'; cargo test --release -p gamersequalizer-mic-vst --lib capture_replay -- --ignored --nocapture
  #[test]
  #[ignore]
  fn capture_replay() {
    let Ok(dir) = std::env::var("AEC_CAPTURE") else {
      return eprintln!("skipped: set AEC_CAPTURE to a debug capture directory");
    };
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(dir);
    let read = |name: &str| -> Vec<f32> {
      hound::WavReader::open(dir.join(name)).unwrap().samples::<f32>().map(|s| s.unwrap()).collect()
    };
    let (mic, mut refs, live) = (read("01_mic_raw.wav"), read("03_ref_aec.wav"), read("04_after_aec.wav"));
    // AEC_OFFSET=<samples>: feed 02_ref_at_mic at one fixed shift instead of the recorded one
    // (compares a held reference window against the live one that followed every refinement).
    if let Some(offset) = std::env::var("AEC_OFFSET").ok().and_then(|v| v.parse::<usize>().ok()) {
      let at_mic = read("02_ref_at_mic.wav");
      refs = (0..mic.len()).map(|n| delayed(&at_mic, n, offset)).collect();
      eprintln!("reference: 02_ref_at_mic shifted by a fixed {offset} samples");
    }
    let t0 = std::time::Instant::now();
    let (out, near_end) = run(&mut AcousticEchoCanceller::default_48k(), &refs, &mic, 480);
    let cpu = t0.elapsed().as_secs_f32() / (mic.len() as f32 / 48_000.0);
    // AEC_OUT=<file.wav>: keep the replayed output for a closer look (e.g. echo correlation).
    if let Ok(path) = std::env::var("AEC_OUT") {
      let spec = hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
      let mut w = hound::WavWriter::create(path, spec).unwrap();
      out.iter().for_each(|&s| w.write_sample(s).unwrap());
      w.finalize().unwrap();
    }
    eprintln!("   t      mic     live      new      ref    (dBFS per 5 s)");
    for t in (0..mic.len()).step_by(5 * 48_000) {
      let r = t..(t + 5 * 48_000).min(mic.len());
      let s = |v: &[f32]| energy_db(&v[r.clone()]);
      eprintln!("{:4} {:8.1} {:8.1} {:8.1} {:8.1}", t / 48_000, s(&mic), s(&live), s(&out), s(&refs));
    }
    let dt = near_end.iter().filter(|&&d| d).count() as f32 / near_end.len().max(1) as f32;
    eprintln!("cpu {:.3} % of a core, near-end share {:.0} %", cpu * 100.0, dt * 100.0);

    let room = mic[..5 * 48_000].to_vec();
    let syn: Vec<f32> = (0..mic.len()).map(|n| delayed(&refs, n, 46) * 0.01 + room[n % room.len()]).collect();
    let (out, _) = run(&mut AcousticEchoCanceller::default_48k(), &refs, &syn, 480);
    let r = 10 * 48_000..50 * 48_000;
    eprintln!("control: input {:.1} -> output {:.1} dB (room noise alone {:.1})", energy_db(&syn[r.clone()]), energy_db(&out[r]), energy_db(&room));
  }

  /// Runs `mic` through the AEC in blocks; returns the output and the double-talk flag per block.
  fn run(aec: &mut AcousticEchoCanceller, refs: &[f32], mic: &[f32], block: usize) -> (Vec<f32>, Vec<bool>) {
    let mut out = Vec::with_capacity(mic.len());
    let mut near_end = Vec::new();
    for (r, m) in refs.chunks(block).zip(mic.chunks(block)) {
      let mut b = m.to_vec();
      aec.process_block(r, &mut b);
      out.extend_from_slice(&b);
      near_end.push(aec.is_double_talk());
    }
    (out, near_end)
  }

  fn energy_db(x: &[f32]) -> f32 {
    10.0 * (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64 + 1e-20).log10() as f32
  }

  fn delayed(x: &[f32], n: usize, by: usize) -> f32 {
    n.checked_sub(by).and_then(|i| x.get(i)).copied().unwrap_or(0.0)
  }

  /// 120 Hz glottal pulses through a resonance that moves every 80 ms, in 4 Hz syllables.
  fn speech_like(len: usize, seed: u32) -> Vec<f32> {
    const FORMANTS: [f32; 5] = [700.0, 1200.0, 400.0, 2400.0, 900.0];
    let white = noise(len, seed);
    let (mut y1, mut y2) = (0.0f32, 0.0f32);
    (0..len)
      .map(|n| {
        let f = FORMANTS[(n / 3_840) % FORMANTS.len()];
        let (rad, theta) = (0.985f32, 2.0 * std::f32::consts::PI * f / 48_000.0);
        let pulse = if n % 400 == 0 { 1.0 } else { 0.0 } + white[n] * 0.05;
        let y = pulse + 2.0 * rad * theta.cos() * y1 - rad * rad * y2;
        (y2, y1) = (y1, y);
        y * 0.02 * (n as f32 * 2.0 * std::f32::consts::PI * 4.0 / 48_000.0).sin().max(0.0)
      })
      .collect()
  }

  fn noise(len: usize, seed: u32) -> Vec<f32> {
    let mut x = seed;
    (0..len)
      .map(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.5
      })
      .collect()
  }
}
