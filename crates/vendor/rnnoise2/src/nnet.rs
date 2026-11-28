//! The 0.2 network: linear layers (f32 dense, int8 dense, int8 sparse 8x4), causal conv1d, GRU
//! and the approximated activations, as upstream's generic C path (`nnet.c`, `nnet_arch.h`,
//! `vec.h` without NEON/AVX) computes them, summation order included.

use crate::bands::NB_BANDS;
use crate::kernel::{Int8Matrix, MAX_GROUPS, MAX_OUTPUTS};

/// Feature vector length (`NB_FEATURES`, `CONV1_IN_SIZE`).
pub(crate) const CONV1_IN: usize = 65;
pub(crate) const CONV1_OUT: usize = 128;
pub(crate) const CONV2_OUT: usize = 384;
pub(crate) const GRU_SIZE: usize = 384;
/// Conv1d kernel width in frames (`nb_inputs / input_size`).
pub(crate) const KERNEL: usize = 3;
/// `cat = [conv2 | gru1 | gru2 | gru3]`, the input of both output layers.
pub(crate) const DENSE_IN: usize = CONV2_OUT + 3 * GRU_SIZE;
/// Largest input of a conv1d layer (`[mem | input]`) or an int8 layer.
const MAX_INPUTS: usize = KERNEL * CONV1_OUT;
const MAX_GRU_OUTPUTS: usize = 3 * GRU_SIZE;
const _: () = assert!(MAX_GROUPS * 4 == MAX_INPUTS && MAX_OUTPUTS == MAX_GRU_OUTPUTS);

/// The weights of a linear layer.
pub(crate) enum Weights {
  /// Column-major f32: `w[j * nb_outputs + i]` is input `j` to output `i`.
  Float(Box<[f32]>),
  /// int8 blocks of 8 outputs x 4 inputs (dense or sparse), see `kernel.rs`.
  Int8(Int8Matrix),
}

/// `LinearLayer`: `out = W x * scale + bias (+ diag)`. Sizes are validated by the loader.
pub(crate) struct Linear {
  nb_inputs: usize,
  nb_outputs: usize,
  weights: Weights,
  /// Per-output scale of the int8 weights (`None` for f32 weights).
  scale: Option<Box<[f32]>>,
  bias: Box<[f32]>,
  /// GRU recurrent weights only: `3 * nb_inputs` diagonal terms.
  diag: Option<Box<[f32]>>,
}

/// All layers of one model (`RNNoise` in `rnnoise_data.h`).
pub(crate) struct RnnModel {
  pub(crate) conv1: Linear,
  pub(crate) conv2: Linear,
  /// (input, recurrent) weights of each GRU.
  pub(crate) gru1: (Linear, Linear),
  pub(crate) gru2: (Linear, Linear),
  pub(crate) gru3: (Linear, Linear),
  pub(crate) dense_out: Linear,
  pub(crate) vad_dense: Linear,
}

/// Recurrent state (`RNNState`): conv memories (previous two input frames, oldest first) and GRU states.
#[derive(Clone)]
pub(crate) struct RnnState {
  conv1: [f32; (KERNEL - 1) * CONV1_IN],
  conv2: [f32; (KERNEL - 1) * CONV1_OUT],
  gru1: [f32; GRU_SIZE],
  gru2: [f32; GRU_SIZE],
  gru3: [f32; GRU_SIZE],
}

impl Default for RnnState {
  fn default() -> Self {
    RnnState {
      conv1: [0.0; (KERNEL - 1) * CONV1_IN],
      conv2: [0.0; (KERNEL - 1) * CONV1_OUT],
      gru1: [0.0; GRU_SIZE],
      gru2: [0.0; GRU_SIZE],
      gru3: [0.0; GRU_SIZE],
    }
  }
}

impl RnnModel {
  /// `compute_rnn`: one frame of features in, band gains out; returns the voice probability.
  pub(crate) fn compute(&self, state: &mut RnnState, gains: &mut [f32; NB_BANDS], features: &[f32; CONV1_IN]) -> f32 {
    let mut cat = [0.0f32; DENSE_IN];
    self.trunk(state, &mut cat, features);
    self.dense_out.compute(gains, &cat);
    gains.iter_mut().for_each(|g| *g = sigmoid_approx(*g));
    self.vad(&cat)
  }

  /// `compute_rnn` without `dense_out`: advances `state` identically and returns the voice probability.
  pub(crate) fn compute_vad(&self, state: &mut RnnState, features: &[f32; CONV1_IN]) -> f32 {
    let mut cat = [0.0f32; DENSE_IN];
    self.trunk(state, &mut cat, features);
    self.vad(&cat)
  }

  #[inline(always)]
  fn trunk(&self, state: &mut RnnState, cat: &mut [f32; DENSE_IN], features: &[f32; CONV1_IN]) {
    let mut tmp = [0.0f32; CONV1_OUT];
    conv1d(&self.conv1, &mut tmp, &mut state.conv1, features);
    if let [conv2, cat1, cat2, cat3] = cat.as_chunks_mut::<GRU_SIZE>().0 {
      conv1d(&self.conv2, conv2, &mut state.conv2, &tmp);
      gru(&self.gru1.0, &self.gru1.1, &mut state.gru1, conv2);
      gru(&self.gru2.0, &self.gru2.1, &mut state.gru2, &state.gru1);
      gru(&self.gru3.0, &self.gru3.1, &mut state.gru3, &state.gru2);
      *cat1 = state.gru1;
      *cat2 = state.gru2;
      *cat3 = state.gru3;
    }
  }

  #[inline(always)]
  fn vad(&self, cat: &[f32; DENSE_IN]) -> f32 {
    let mut vad = [0.0f32; 1];
    self.vad_dense.compute(&mut vad, cat);
    let [vad] = vad;
    sigmoid_approx(vad)
  }
}

impl Linear {
  pub(crate) fn new(
    nb_inputs: usize,
    nb_outputs: usize,
    weights: Weights,
    scale: Option<Box<[f32]>>,
    bias: Box<[f32]>,
    diag: Option<Box<[f32]>>,
  ) -> Self {
    Linear { nb_inputs, nb_outputs, weights, scale, bias, diag }
  }

  /// `compute_linear`: `out` holds `nb_outputs` values, `input` `nb_inputs` (excess is ignored,
  /// a short slice leaves the tail of `out` partial; the callers use exact fixed sizes).
  pub(crate) fn compute(&self, out: &mut [f32], input: &[f32]) {
    let out = out.get_mut(..self.nb_outputs).unwrap_or_default();
    let input = input.get(..self.nb_inputs).unwrap_or_default();
    out.fill(0.0);
    match &self.weights {
      Weights::Float(w) => sgemv(out, w, input),
      Weights::Int8(m) => {
        // The exact integer sums equal the C's f32 accumulation (see `kernel.rs`).
        let mut acc = [0i32; MAX_OUTPUTS];
        m.gemv(&mut acc, &quantize(input));
        out.iter_mut().zip(acc).for_each(|(o, a)| *o = a as f32);
      }
    }
    if let Some(scale) = &self.scale {
      out.iter_mut().zip(scale.iter()).for_each(|(o, s)| *o *= s);
    }
    out.iter_mut().zip(self.bias.iter()).for_each(|(o, b)| *o += b);
    if let Some(diag) = &self.diag {
      // out[i + k*M] += diag[i + k*M] * in[i], k = 0..3 (GRU recurrent weights only).
      for (o, d) in out.chunks_exact_mut(input.len().max(1)).zip(diag.chunks_exact(input.len().max(1))) {
        for ((o, d), x) in o.iter_mut().zip(d).zip(input) {
          *o += d * x;
        }
      }
    }
  }
}

/// `compute_generic_conv1d` with tanh: `tmp = [mem | input]`, `out = tanh(linear(tmp))`,
/// `mem = tmp[input.len()..]`.
pub(crate) fn conv1d(layer: &Linear, out: &mut [f32], mem: &mut [f32], input: &[f32]) {
  let mut tmp = [0.0f32; MAX_INPUTS];
  let len = (mem.len() + input.len()).min(MAX_INPUTS);
  tmp.iter_mut().zip(mem.iter().chain(input)).for_each(|(t, v)| *t = *v);
  let tmp = tmp.get(..len).unwrap_or_default();
  layer.compute(out, tmp);
  out.iter_mut().for_each(|o| *o = tanh_approx(*o));
  mem.iter_mut().zip(tmp.iter().skip(input.len())).for_each(|(m, t)| *m = *t);
}

/// `compute_generic_gru`, gates z, r, h: `z, r = sigmoid(W x + R s)`, `h = tanh(W_h x + r * R_h s)`,
/// `s = z * s + (1 - z) * h`.
pub(crate) fn gru(input_weights: &Linear, recurrent_weights: &Linear, state: &mut [f32], input: &[f32]) {
  let n = state.len().clamp(1, GRU_SIZE);
  let mut zrh = [0.0f32; MAX_GRU_OUTPUTS];
  let mut recur = [0.0f32; MAX_GRU_OUTPUTS];
  input_weights.compute(&mut zrh, input);
  recurrent_weights.compute(&mut recur, state);
  let (zr, h) = zrh.split_at_mut(2 * n);
  let (recur_zr, recur_h) = recur.split_at(2 * n);
  for (a, b) in zr.iter_mut().zip(recur_zr) {
    *a = sigmoid_approx(*a + b);
  }
  let (z, r) = zr.split_at(n);
  for ((h, rh), r) in h.iter_mut().zip(recur_h).zip(r) {
    *h = tanh_approx(*h + rh * r);
  }
  for ((s, h), z) in state.iter_mut().zip(h.iter()).zip(z) {
    *s = z * *s + (1.0 - z) * h;
  }
}

/// `sgemv` (all three generic variants): per output, `sum_j w[j*N + i] * x[j]` from 0 in `j` order.
fn sgemv(out: &mut [f32], w: &[f32], input: &[f32]) {
  // Fixed widths keep the accumulators in registers (same per-output order, same result).
  if let Ok(out) = <&mut [f32; 1]>::try_from(&mut *out) {
    return sgemv_fixed(out, w, input);
  }
  if let Ok(out) = <&mut [f32; NB_BANDS]>::try_from(&mut *out) {
    return sgemv_fixed(out, w, input);
  }
  if let Ok(out) = <&mut [f32; CONV1_OUT]>::try_from(&mut *out) {
    return sgemv_fixed(out, w, input);
  }
  for (column, &x) in w.chunks_exact(out.len().max(1)).zip(input) {
    for (o, &w) in out.iter_mut().zip(column) {
      *o += w * x;
    }
  }
}

fn sgemv_fixed<const N: usize>(out: &mut [f32; N], w: &[f32], input: &[f32]) {
  let mut acc = *out;
  for (column, &x) in w.as_chunks::<N>().0.iter().zip(input) {
    for (o, &w) in acc.iter_mut().zip(column) {
      *o += w * x;
    }
  }
  *out = acc;
}

/// Input quantization of `cgemv8x4`: `(int)floor(.5 + 127.f * x)` (product in f32, rounding in f64).
fn quantize(input: &[f32]) -> [[i8; 4]; MAX_GROUPS] {
  let mut xq = [[0i8; 4]; MAX_GROUPS];
  for (q, &x) in xq.as_flattened_mut().iter_mut().zip(input) {
    // Inputs are tanh/GRU outputs within [-1, 1], so the value fits i8 (C wraps the same way).
    *q = floor_i32(0.5f64 + (127.0f32 * x) as f64) as i8;
  }
  xq
}

/// `x.floor() as i32` (saturating, NaN to 0) without `f64::floor`, which is a libm call on x86_64
/// without SSE4.1 (the Windows baseline): truncate, then step down where that rounded up.
#[inline(always)]
fn floor_i32(x: f64) -> i32 {
  let t = x as i32;
  if t as f64 > x {
    t.saturating_sub(1)
  } else {
    t
  }
}

/// `tanh_approx` of `vec.h` (rational approximation, f32, no FMA), clamped to [-1, 1].
#[allow(clippy::excessive_precision, reason = "coefficients copied verbatim from upstream vec.h")]
pub(crate) fn tanh_approx(x: f32) -> f32 {
  const N0: f32 = 952.52801514;
  const N1: f32 = 96.39235687;
  const N2: f32 = 0.60863042;
  const D0: f32 = 952.72399902;
  const D1: f32 = 413.36801147;
  const D2: f32 = 11.88600922;
  let x2 = x * x;
  let num = (N2 * x2 + N1) * x2 + N0;
  let den = (D2 * x2 + D1) * x2 + D0;
  let y = num * x / den;
  // MAX32(-1, MIN32(1, y)), NaN passes through like the C macros.
  let y = if 1.0 < y { 1.0 } else { y };
  if -1.0 > y {
    -1.0
  } else {
    y
  }
}

/// `sigmoid_approx` of `vec.h`: `0.5 + 0.5 * tanh_approx(0.5 * x)`.
pub(crate) fn sigmoid_approx(x: f32) -> f32 {
  0.5 + 0.5 * tanh_approx(0.5 * x)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used, reason = "tests")]
mod tests {
  use super::*;

  fn float(nb_in: usize, nb_out: usize, w: &[f32], bias: &[f32]) -> Linear {
    Linear::new(nb_in, nb_out, Weights::Float(w.into()), None, bias.into(), None)
  }

  #[test]
  fn activations_match_reference_points() {
    assert_eq!(tanh_approx(0.0), 0.0);
    assert_eq!(tanh_approx(10.0), 1.0);
    assert_eq!(tanh_approx(-10.0), -1.0);
    assert!((tanh_approx(0.5) - 0.5f32.tanh()).abs() < 1e-4);
    assert_eq!(sigmoid_approx(0.0), 0.5);
  }

  #[test]
  fn float_dense_is_column_major() {
    // 3 inputs, 2 outputs: w[j*2 + i].
    let layer = float(3, 2, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], &[0.5, -0.5]);
    let mut out = [0.0; 2];
    layer.compute(&mut out, &[1.0, 10.0, 100.0]);
    assert_eq!(out, [1.0 + 30.0 + 500.0 + 0.5, 2.0 + 40.0 + 600.0 - 0.5]);
  }

  #[test]
  fn int8_dense_quantizes_and_scales() {
    // 8 inputs (two input groups), 8 outputs. Weight of output k, input j: (k + 1) * (j - 3).
    let mut w = Vec::new();
    for j0 in [0, 4] {
      for k in 0..8i32 {
        for c in 0..4 {
          w.push(((k + 1) * (j0 + c - 3)) as i8 as u8);
        }
      }
    }
    let w: &'static [u8] = Box::leak(w.into_boxed_slice());
    // x = q / 127 quantizes back to q exactly; 0.3/127 rounds to 0.
    let q = [1i32, -2, 3, 0, 5, 127, -127, 0];
    let mut x: Vec<f32> = q.iter().map(|&q| q as f32 / 127.0).collect();
    x[3] = 0.3 / 127.0;
    let scale: Vec<f32> = (0..8).map(|k| 0.25 * (k + 1) as f32).collect();
    let bias = [1.0; 8];
    let layer = Linear::new(
      8,
      8,
      Weights::Int8(Int8Matrix::dense(w, 8, 8).unwrap()),
      Some(scale.clone().into()),
      bias.into(),
      None,
    );
    let mut out = [0.0; 8];
    layer.compute(&mut out, &x);
    for k in 0..8 {
      let sum: i32 = (0..8).map(|j| (k as i32 + 1) * (j - 3) * q[j as usize]).sum();
      assert_eq!(out[k], sum as f32 * scale[k] + 1.0, "output {k}");
    }
  }

  #[test]
  fn int8_sparse_uses_listed_blocks_and_diag() {
    // 8 inputs, 24 outputs (3 groups of 8) = GRU-shaped recurrent layer with N = 8.
    // Group 0: block at input 4; group 1: no block; group 2: blocks at 0 and 4.
    let idx = [1, 4, 0, 2, 0, 4];
    let mut w = Vec::new();
    for b in 0..3i32 {
      for k in 0..8 {
        for c in 0..4 {
          w.push((b + k - c) as i8 as u8);
        }
      }
    }
    let w: &'static [u8] = Box::leak(w.into_boxed_slice());
    let q = [2i32, -1, 0, 4, 1, 1, -3, 7];
    let x: Vec<f32> = q.iter().map(|&q| q as f32 / 127.0).collect();
    let diag: Vec<f32> = (0..24).map(|i| i as f32 * 0.5).collect();
    let layer = Linear::new(
      8,
      24,
      Weights::Int8(Int8Matrix::sparse(w, &idx, 8, 24).unwrap()),
      Some(vec![1.0; 24].into()),
      vec![0.0; 24].into(),
      Some(diag.clone().into()),
    );
    let mut out = [0.0; 24];
    layer.compute(&mut out, &x);
    let block =
      |b: i32, k: i32, j0: usize| -> f32 { (0..4).map(|c| (b + k - c) * q[j0 + c as usize]).sum::<i32>() as f32 };
    for i in 0..24 {
      let k = (i % 8) as i32;
      let expected = match i / 8 {
        0 => block(0, k, 4),
        1 => 0.0,
        _ => block(1, k, 0) + block(2, k, 4),
      };
      assert_eq!(out[i], expected + diag[i] * x[i % 8], "output {i}");
    }
  }

  #[test]
  fn conv1d_keeps_two_frames_oldest_first() {
    // input_size 1, kernel 3: out = tanh(a*x[t-2] + b*x[t-1] + c*x[t] + bias).
    let (a, b, c, bias) = (0.1, 0.2, 0.3, 0.05);
    let layer = float(3, 1, &[a, b, c], &[bias]);
    let mut mem = [0.0; 2];
    let mut out = [0.0; 1];
    conv1d(&layer, &mut out, &mut mem, &[1.0]);
    assert_eq!(out[0], tanh_approx(c * 1.0 + bias));
    conv1d(&layer, &mut out, &mut mem, &[2.0]);
    assert_eq!(out[0], tanh_approx(b * 1.0 + c * 2.0 + bias));
    assert_eq!(mem, [1.0, 2.0]);
  }

  #[test]
  fn gru_step_matches_formula() {
    // N = 1: input weights (1 in, 3 out), recurrent (1 in, 3 out).
    let (wz, wr, wh, bz, br, bh) = (0.5, -0.25, 1.5, 0.1, 0.2, -0.3);
    let (uz, ur, uh, cz, cr, ch) = (0.75, 0.5, -1.0, 0.0, 0.05, 0.1);
    let input = float(1, 3, &[wz, wr, wh], &[bz, br, bh]);
    let recurrent = float(1, 3, &[uz, ur, uh], &[cz, cr, ch]);
    let (x, s0) = (0.8f32, 0.4f32);
    let mut state = [s0];
    gru(&input, &recurrent, &mut state, &[x]);
    let z = sigmoid_approx((wz * x + bz) + (uz * s0 + cz));
    let r = sigmoid_approx((wr * x + br) + (ur * s0 + cr));
    let h = tanh_approx((wh * x + bh) + (uh * s0 + ch) * r);
    assert_eq!(state[0], z * s0 + (1.0 - z) * h);
  }

  #[test]
  fn floor_i32_matches_floor() {
    let mut special = vec![0.0, -0.0, 0.5, -0.5, 1.0, -1.0, 127.5, -127.5, -126.5, 2.5e9, -2.5e9, -2147483648.5];
    special.extend([f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX, f64::MIN, 1e-300, -1e-300]);
    let mut seed = 0x2545_f491u32;
    let random = (0..100_000).map(|_| {
      seed ^= seed << 13;
      seed ^= seed >> 17;
      seed ^= seed << 5;
      // The quantizer's real input: 0.5 + 127 * x for x in about [-1.1, 1.1].
      0.5 + (127.0f32 * (seed as f32 / u32::MAX as f32 * 2.2 - 1.1)) as f64
    });
    for x in special.into_iter().chain(random) {
      assert_eq!(floor_i32(x), x.floor() as i32, "{x}");
    }
  }

  /// Per-layer cost of one frame (`--release -- --ignored --nocapture stage_timing`).
  #[test]
  #[ignore = "timing report, run with --release"]
  fn stage_timing() {
    use std::hint::black_box;
    use std::time::Instant;
    const N: u32 = 2000;
    for model in [crate::Model::Full, crate::Model::Little] {
      let m = crate::weights::load_model(model.blob()).unwrap();
      let x: Vec<f32> = (0..DENSE_IN).map(|i| ((i as f32) * 0.37).sin() * 0.9).collect();
      let mut out = [0.0f32; MAX_GRU_OUTPUTS];
      let mut time = |name: &str, f: &mut dyn FnMut(&mut [f32])| {
        let start = Instant::now();
        for _ in 0..N {
          f(black_box(&mut out));
        }
        println!("{model:?} {name:12} {:8.0} ns", start.elapsed().as_nanos() as f64 / N as f64);
      };
      time("conv1", &mut |o| m.conv1.compute(o, &x));
      time("conv2", &mut |o| m.conv2.compute(o, &x));
      for (i, g) in [&m.gru1, &m.gru2, &m.gru3].iter().enumerate() {
        time(&format!("gru{}_in", i + 1), &mut |o| g.0.compute(o, &x));
        time(&format!("gru{}_rec", i + 1), &mut |o| g.1.compute(o, &x));
      }
      time("dense_out", &mut |o| m.dense_out.compute(o, &x));
      time("vad_dense", &mut |o| m.vad_dense.compute(o, &x));
      let mut st = RnnState::default();
      let mut g = [0.0f32; NB_BANDS];
      let f: [f32; CONV1_IN] = std::array::from_fn(|i| x[i]);
      time("compute_rnn", &mut |_| {
        black_box(m.compute(&mut st, &mut g, &f));
      });
      time("compute_vad", &mut |_| {
        black_box(m.compute_vad(&mut st, &f));
      });
    }
  }
}
