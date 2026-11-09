//! Pitch analysis (`pitch.c`, `celt_lpc.c`, float build): downsampling with an LPC whitening
//! filter, coarse-to-fine period search and `remove_doubling`.
//!
//! Algorithmically the same as nnnoiseless 0.5.2 `pitch.rs`, but every sum here follows the C
//! order: inner products are one sequential sum (nnnoiseless uses four partial sums), the
//! `yy` update is `(yy + a) - b`, `find_best_pitch` scales by `1e-12` before squaring, and the
//! pitch gain divides by a double `sqrt`. The network amplifies last-bit differences, so these matter.

// Every index is bounded by the PITCH_* constants on fixed-size arrays; the only data-dependent
// offsets (periods) are clamped to [PITCH_MIN_PERIOD / 2, PITCH_MAX_PERIOD / 2) by construction.
#![allow(clippy::indexing_slicing)]

pub(crate) const PITCH_MIN_PERIOD: usize = 60;
pub(crate) const PITCH_MAX_PERIOD: usize = 768;
pub(crate) const PITCH_FRAME_SIZE: usize = 960;
pub(crate) const PITCH_BUF_SIZE: usize = PITCH_MAX_PERIOD + PITCH_FRAME_SIZE;
const LP_SIZE: usize = PITCH_BUF_SIZE / 2;
const SECOND_CHECK: [usize; 16] = [0, 0, 3, 2, 3, 2, 5, 2, 3, 2, 3, 2, 5, 2, 3, 2];

/// Pitch tracking memory between frames.
#[derive(Clone, Copy, Default)]
pub(crate) struct PitchState {
  last_period: usize,
  last_gain: f32,
}

impl PitchState {
  /// The pitch period of the newest `PITCH_FRAME_SIZE` samples of `buf`, in samples, within
  /// `[PITCH_MIN_PERIOD, PITCH_MAX_PERIOD)` (the downsample/search/remove_doubling sequence of
  /// `rnn_compute_frame_features`).
  pub(crate) fn find(&mut self, buf: &[f32; PITCH_BUF_SIZE]) -> usize {
    let mut lp = [0.0f32; LP_SIZE];
    pitch_downsample(buf, &mut lp);
    let search = pitch_search(&lp);
    let mut pitch_index = PITCH_MAX_PERIOD.saturating_sub(search);
    let gain = remove_doubling(&lp, &mut pitch_index, self.last_period, self.last_gain);
    self.last_period = pitch_index;
    self.last_gain = gain;
    pitch_index.min(PITCH_MAX_PERIOD - 1)
  }
}

/// `rnn_pitch_downsample` (one channel): 2x decimation, then a 4th-order LPC whitening FIR plus a zero.
fn pitch_downsample(x: &[f32; PITCH_BUF_SIZE], x_lp: &mut [f32; LP_SIZE]) {
  for i in 1..LP_SIZE {
    x_lp[i] = 0.5 * (0.5 * (x[2 * i - 1] + x[2 * i + 1]) + x[2 * i]);
  }
  x_lp[0] = 0.5 * (0.5 * x[1] + x[0]);
  let mut ac = autocorr(x_lp);
  // Noise floor -40 dB.
  ac[0] *= 1.0001;
  // Lag windowing.
  for (i, a) in ac.iter_mut().enumerate().skip(1) {
    let w = 0.008f32 * i as f32;
    *a -= *a * w * w;
  }
  let mut lpc = lpc(&ac);
  let mut tmp = 1.0f32;
  for l in &mut lpc {
    tmp *= 0.9;
    *l *= tmp;
  }
  // Add a zero.
  let c1 = 0.8f32;
  let lpc2 = [lpc[0] + 0.8, lpc[1] + c1 * lpc[0], lpc[2] + c1 * lpc[1], lpc[3] + c1 * lpc[2], c1 * lpc[3]];
  fir5_in_place(x_lp, &lpc2);
}

/// `rnn_autocorr` with lag 4, no window: `ac[k] = sum_i x[i] * x[i + k]`, summed as upstream
/// (the `fastN = n - lag` part first, then the tail).
fn autocorr(x: &[f32; LP_SIZE]) -> [f32; 5] {
  const LAG: usize = 4;
  let fast_n = LP_SIZE - LAG;
  let mut ac = [0.0f32; LAG + 1];
  for (k, a) in ac.iter_mut().enumerate() {
    *a = inner_prod(&x[..fast_n], &x[k..k + fast_n]);
    let mut d = 0.0f32;
    for i in k + fast_n..LP_SIZE {
      d += x[i] * x[i - k];
    }
    *a += d;
  }
  ac
}

/// `rnn_lpc`, order 4 (float build).
fn lpc(ac: &[f32; 5]) -> [f32; 4] {
  let mut lpc = [0.0f32; 4];
  let mut error = ac[0];
  if ac[0] == 0.0 {
    return lpc;
  }
  for i in 0..4 {
    let mut rr = 0.0f32;
    for j in 0..i {
      rr += lpc[j] * ac[i - j];
    }
    rr += ac[i + 1];
    let r = -rr / error;
    lpc[i] = r;
    for j in 0..(i + 1) >> 1 {
      let tmp1 = lpc[j];
      let tmp2 = lpc[i - 1 - j];
      lpc[j] = tmp1 + r * tmp2;
      lpc[i - 1 - j] = tmp2 + r * tmp1;
    }
    error -= r * r * error;
    if error < 0.001 * ac[0] {
      break;
    }
  }
  lpc
}

/// `celt_fir5` in place with zero memory.
fn fir5_in_place(x: &mut [f32], num: &[f32; 5]) {
  let mut mem = [0.0f32; 5];
  for v in x {
    let sum = *v + num[0] * mem[0] + num[1] * mem[1] + num[2] * mem[2] + num[3] * mem[3] + num[4] * mem[4];
    mem = [*v, mem[0], mem[1], mem[2], mem[3]];
    *v = sum;
  }
}

/// `rnn_pitch_search` on the 2x-decimated buffer; returns the lag of the best match.
fn pitch_search(lp: &[f32; LP_SIZE]) -> usize {
  const LEN: usize = PITCH_FRAME_SIZE;
  const MAX_PITCH: usize = PITCH_MAX_PERIOD - 3 * PITCH_MIN_PERIOD;
  let x_lp = &lp[PITCH_MAX_PERIOD / 2..];
  let y = &lp[..];
  let mut x_lp4 = [0.0f32; LEN / 4];
  let mut y_lp4 = [0.0f32; (LEN + MAX_PITCH) / 4];
  for (j, v) in x_lp4.iter_mut().enumerate() {
    *v = x_lp[2 * j];
  }
  for (j, v) in y_lp4.iter_mut().enumerate() {
    *v = y[2 * j];
  }
  // Coarse search with 4x decimation (`rnn_pitch_xcorr`: per lag one sequential sum, which is
  // what the unrolled `xcorr_kernel` computes for each of its four accumulators).
  let mut xcorr = [0.0f32; MAX_PITCH / 2];
  pitch_xcorr(&x_lp4, &y_lp4, &mut xcorr[..MAX_PITCH / 4]);
  let best = find_best_pitch(&xcorr[..MAX_PITCH / 4], &y_lp4, LEN / 4);
  // Finer search with 2x decimation.
  for i in 0..MAX_PITCH / 2 {
    xcorr[i] = 0.0;
    if (i as isize - 2 * best[0] as isize).abs() > 2 && (i as isize - 2 * best[1] as isize).abs() > 2 {
      continue;
    }
    let sum = inner_prod(&x_lp[..LEN / 2], &y[i..i + LEN / 2]);
    xcorr[i] = if -1.0 > sum { -1.0 } else { sum };
  }
  let best = find_best_pitch(&xcorr, y, LEN / 2);
  // Refine by pseudo-interpolation.
  let mut offset = 0isize;
  if best[0] > 0 && best[0] < MAX_PITCH / 2 - 1 {
    let (a, b, c) = (xcorr[best[0] - 1], xcorr[best[0]], xcorr[best[0] + 1]);
    if c - a > 0.7 * (b - a) {
      offset = 1;
    } else if a - c > 0.7 * (b - c) {
      offset = -1;
    }
  }
  (2 * best[0] as isize - offset).max(0) as usize
}

/// `find_best_pitch` (float build): the two lags with the largest `xcorr^2 / energy(y)`.
fn find_best_pitch(xcorr: &[f32], y: &[f32], len: usize) -> [usize; 2] {
  let mut syy = 1.0f32;
  let mut best_num = [-1.0f32; 2];
  let mut best_den = [0.0f32; 2];
  let mut best_pitch = [0usize, 1];
  for &v in &y[..len] {
    syy += v * v;
  }
  for (i, &corr) in xcorr.iter().enumerate() {
    if corr > 0.0 {
      // Avoids under/overflow when squaring (upstream's float-only scaling).
      let xcorr16 = corr * 1e-12;
      let num = xcorr16 * xcorr16;
      if num * best_den[1] > best_num[1] * syy {
        if num * best_den[0] > best_num[0] * syy {
          best_num[1] = best_num[0];
          best_den[1] = best_den[0];
          best_pitch[1] = best_pitch[0];
          best_num[0] = num;
          best_den[0] = syy;
          best_pitch[0] = i;
        } else {
          best_num[1] = num;
          best_den[1] = syy;
          best_pitch[1] = i;
        }
      }
    }
    syy += y[i + len] * y[i + len] - y[i] * y[i];
    syy = if 1.0 > syy { 1.0 } else { syy };
  }
  best_pitch
}

/// `rnn_remove_doubling`: checks the submultiples T/k of the found period and keeps the shortest
/// one that correlates well enough. Updates `t0` (full-rate period) and returns the pitch gain.
fn remove_doubling(lp: &[f32; LP_SIZE], t0: &mut usize, prev_period: usize, prev_gain: f32) -> f32 {
  const MAX_PERIOD: usize = PITCH_MAX_PERIOD / 2;
  const MIN_PERIOD: usize = PITCH_MIN_PERIOD / 2;
  const N: usize = PITCH_FRAME_SIZE / 2;
  // `x` in the C is `lp + MAX_PERIOD`; `at(d)` is `x - d`.
  let x = &lp[MAX_PERIOD..MAX_PERIOD + N];
  let at = |d: usize| &lp[MAX_PERIOD - d..MAX_PERIOD - d + N];
  let prev_period = prev_period / 2;
  let t0_half = (*t0 / 2).min(MAX_PERIOD - 1);
  let mut t = t0_half;
  let xx = inner_prod(x, x);
  let mut xy = inner_prod(x, at(t0_half));
  let mut yy_lookup = [0.0f32; MAX_PERIOD + 1];
  yy_lookup[0] = xx;
  let mut yy = xx;
  for i in 1..=MAX_PERIOD {
    yy = yy + lp[MAX_PERIOD - i] * lp[MAX_PERIOD - i] - lp[MAX_PERIOD + N - i] * lp[MAX_PERIOD + N - i];
    yy_lookup[i] = if 0.0 > yy { 0.0 } else { yy };
  }
  yy = yy_lookup[t0_half];
  let mut best_xy = xy;
  let mut best_yy = yy;
  let g0 = pitch_gain(xy, xx, yy);
  let mut g = g0;
  // Look for any pitch at T/k.
  for (k, &second_check) in SECOND_CHECK.iter().enumerate().skip(2) {
    let t1 = (2 * t0_half + k) / (2 * k);
    if t1 < MIN_PERIOD {
      break;
    }
    // Look for another strong correlation at t1b.
    let t1b = if k == 2 {
      if t1 + t0_half > MAX_PERIOD {
        t0_half
      } else {
        t0_half + t1
      }
    } else {
      (2 * second_check * t0_half + k) / (2 * k)
    };
    xy = inner_prod(x, at(t1));
    let xy2 = inner_prod(x, at(t1b));
    xy = 0.5 * (xy + xy2);
    yy = 0.5 * (yy_lookup[t1] + yy_lookup[t1b]);
    let g1 = pitch_gain(xy, xx, yy);
    let cont = if t1.abs_diff(prev_period) <= 1 {
      prev_gain
    } else if t1.abs_diff(prev_period) <= 2 && 5 * k * k < t0_half {
      0.5 * prev_gain
    } else {
      0.0
    };
    let max = |a: f32, b: f32| if a > b { a } else { b };
    let mut thresh = max(0.3, 0.7 * g0 - cont);
    // Bias against very high pitch (very short period) to avoid false-positives due to
    // short-term correlation. (The second branch is unreachable, as upstream.)
    if t1 < 3 * MIN_PERIOD {
      thresh = max(0.4, 0.85 * g0 - cont);
    } else if t1 < 2 * MIN_PERIOD {
      thresh = max(0.5, 0.9 * g0 - cont);
    }
    if g1 > thresh {
      best_xy = xy;
      best_yy = yy;
      t = t1;
      g = g1;
    }
  }
  best_xy = if 0.0 > best_xy { 0.0 } else { best_xy };
  let mut pg = if best_yy <= best_xy { 1.0 } else { best_xy / (best_yy + 1.0) };
  let mut xcorr = [0.0f32; 3];
  for (k, c) in xcorr.iter_mut().enumerate() {
    *c = inner_prod(x, at((t + k).saturating_sub(1)));
  }
  let offset: isize = if xcorr[2] - xcorr[0] > 0.7 * (xcorr[1] - xcorr[0]) {
    1
  } else if xcorr[0] - xcorr[2] > 0.7 * (xcorr[1] - xcorr[2]) {
    -1
  } else {
    0
  };
  if pg > g {
    pg = g;
  }
  *t0 = ((2 * t) as isize + offset).max(PITCH_MIN_PERIOD as isize) as usize;
  pg
}

/// `compute_pitch_gain` (float): `xy / sqrt(1 + xx*yy)`, the `sqrt` and division in double.
fn pitch_gain(xy: f32, xx: f32, yy: f32) -> f32 {
  (xy as f64 / ((1.0 + xx * yy) as f64).sqrt()) as f32
}

/// `xcorr[i] = inner_prod(x, y[i..])` for every lag `i`, 8 lags at a time: each lag is still one
/// sequential sum (the same result), but the 8 dependency chains run side by side.
fn pitch_xcorr(x: &[f32], y: &[f32], xcorr: &mut [f32]) {
  let (lags, rest) = xcorr.as_chunks_mut::<8>();
  for (out, i) in lags.iter_mut().zip((0..).step_by(8)) {
    *out = [0.0; 8];
    for (&xj, w) in x.iter().zip(y[i..].windows(8)) {
      for (a, &yv) in out.iter_mut().zip(w) {
        *a += xj * yv;
      }
    }
  }
  for (o, k) in rest.iter_mut().zip(8 * lags.len()..) {
    *o = inner_prod(x, &y[k..]);
  }
}

/// `celt_inner_prod`: one sequential f32 sum over the shorter of the two slices.
fn inner_prod(x: &[f32], y: &[f32]) -> f32 {
  let mut sum = 0.0f32;
  for (a, b) in x.iter().zip(y) {
    sum += a * b;
  }
  sum
}
