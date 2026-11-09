//! The per-frame pipeline (`rnnoise_process_frame` and helpers in `denoise.c`, `main` @ 70f1d25).

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use crate::bands::{
  band_corr, band_energy, interp_band_gain, Tables, FRAME_SIZE, FREQ_SIZE, NB_BANDS, NB_FEATURES, WINDOW_SIZE,
};
use crate::nnet::{RnnModel, RnnState};
use crate::pitch::{PitchState, PITCH_BUF_SIZE};

/// `rnn_kfft.scale` (1/960 as upstream prints it); kiss scales the FFT input by it.
const FFT_SCALE: f32 = 0.0010416667;
/// High-pass `rnn_biquad` coefficients (`b` and `a` without the leading 1).
const B_HP: [f32; 2] = [-2.0, 1.0];
const A_HP: [f32; 2] = [-1.99599, 0.99600];
/// Below this summed band energy a frame is "silence": the network is not run.
const SILENCE_ENERGY: f64 = 0.04;

const ZERO: Complex32 = Complex32::new(0.0, 0.0);

/// The resettable signal state (`DenoiseState` minus the model).
#[derive(Clone)]
pub(crate) struct Memory {
  analysis_mem: [f32; FRAME_SIZE],
  synthesis_mem: [f32; FRAME_SIZE],
  pitch_buf: [f32; PITCH_BUF_SIZE],
  pitch: PitchState,
  mem_hp_x: [f32; 2],
  lastg: [f32; NB_BANDS],
  rnn: RnnState,
  delayed_x: [Complex32; FREQ_SIZE],
  delayed_p: [Complex32; FREQ_SIZE],
  delayed_ex: [f32; NB_BANDS],
  delayed_ep: [f32; NB_BANDS],
  delayed_exp: [f32; NB_BANDS],
}

impl Memory {
  pub(crate) fn new() -> Self {
    Memory {
      analysis_mem: [0.0; FRAME_SIZE],
      synthesis_mem: [0.0; FRAME_SIZE],
      pitch_buf: [0.0; PITCH_BUF_SIZE],
      pitch: PitchState::default(),
      mem_hp_x: [0.0; 2],
      lastg: [0.0; NB_BANDS],
      rnn: RnnState::default(),
      delayed_x: [ZERO; FREQ_SIZE],
      delayed_p: [ZERO; FREQ_SIZE],
      delayed_ex: [0.0; NB_BANDS],
      delayed_ep: [0.0; NB_BANDS],
      delayed_exp: [0.0; NB_BANDS],
    }
  }
}

/// Analysis of one frame (`rnn_compute_frame_features` outputs).
struct Frame {
  x: [Complex32; FREQ_SIZE],
  p: [Complex32; FREQ_SIZE],
  ex: [f32; NB_BANDS],
  ep: [f32; NB_BANDS],
  exp: [f32; NB_BANDS],
  features: [f32; NB_FEATURES],
}

impl Frame {
  fn new() -> Self {
    Frame {
      x: [ZERO; FREQ_SIZE],
      p: [ZERO; FREQ_SIZE],
      ex: [0.0; NB_BANDS],
      ep: [0.0; NB_BANDS],
      exp: [0.0; NB_BANDS],
      features: [0.0; NB_FEATURES],
    }
  }
}

/// The FFT (960 points, complex, in place) with preallocated buffers.
pub(crate) struct Transform {
  fft: Arc<dyn Fft<f32>>,
  buf: [Complex32; WINDOW_SIZE],
  scratch: Box<[Complex32]>,
}

impl Transform {
  pub(crate) fn new() -> Self {
    let fft = FftPlanner::<f32>::new().plan_fft_forward(WINDOW_SIZE);
    let scratch = vec![ZERO; fft.get_inplace_scratch_len()].into_boxed_slice();
    Transform { fft, buf: [ZERO; WINDOW_SIZE], scratch }
  }

  /// `forward_transform`: DFT of the input scaled by 1/960 (kiss scales the input), bins 0..=480.
  fn forward(&mut self, out: &mut [Complex32; FREQ_SIZE], input: &[f32; WINDOW_SIZE]) {
    for (b, &x) in self.buf.iter_mut().zip(input) {
      *b = Complex32::new(FFT_SCALE * x, 0.0);
    }
    self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
    out.iter_mut().zip(&self.buf).for_each(|(o, b)| *o = *b);
  }

  /// `inverse_transform`: forward FFT (input scaled by 1/960) of the Hermitian-extended spectrum,
  /// read in reverse and times 960, i.e. an unscaled inverse DFT.
  fn inverse(&mut self, out: &mut [f32; WINDOW_SIZE], input: &[Complex32; FREQ_SIZE]) {
    let (low, high) = self.buf.split_at_mut(FREQ_SIZE);
    low.iter_mut().zip(input).for_each(|(b, x)| *b = *x);
    // x[i] = conj(x[960 - i]) for i in 481..960, i.e. bins 479 down to 1.
    high.iter_mut().zip(input.iter().skip(1).rev().skip(1)).for_each(|(b, x)| *b = x.conj());
    for b in &mut self.buf {
      *b = Complex32::new(FFT_SCALE * b.re, FFT_SCALE * b.im);
    }
    self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
    // out[0] = 960 * y[0]; out[i] = 960 * y[960 - i].
    let reversed = self.buf.iter().take(1).chain(self.buf.iter().skip(1).rev());
    out.iter_mut().zip(reversed).for_each(|(o, y)| *o = WINDOW_SIZE as f32 * y.re);
  }
}

/// One `rnnoise_process_frame` call on `input` (480 samples, i16 scale); writes `output` and
/// returns the voice probability.
pub(crate) fn process_frame(
  model: &RnnModel,
  tables: &Tables,
  fft: &mut Transform,
  st: &mut Memory,
  output: &mut [f32; FRAME_SIZE],
  input: &[f32; FRAME_SIZE],
) -> f32 {
  let mut frame = Frame::new();
  let mut g = [0.0f32; NB_BANDS];
  let vad = analyze(model, tables, fft, st, &mut frame, &mut g, input);
  if vad.is_some() {
    pitch_filter(&mut st.delayed_x, &st.delayed_p, &st.delayed_ex, &st.delayed_ep, &st.delayed_exp, &g);
    for (((g, lastg), &dex), &ex) in g.iter_mut().zip(&mut st.lastg).zip(&st.delayed_ex).zip(&frame.ex) {
      // Cap the decay at 0.6 per frame (RT60 of 135 ms).
      let floor = 0.6f32 * *lastg;
      *g = if *g > floor { *g } else { floor };
      // Compensate for the energy change across the frame when computing the threshold gain.
      let next = *g as f64 * (dex as f64 + 1e-3) / (ex as f64 + 1e-3);
      *lastg = if 1.0 < next { 1.0 } else { next as f32 };
    }
    let mut gf = [0.0f32; FREQ_SIZE];
    interp_band_gain(&mut gf, &g);
    for (x, &g) in st.delayed_x.iter_mut().zip(&gf) {
      x.re *= g;
      x.im *= g;
    }
  }
  frame_synthesis(tables, fft, st, output);
  st.delayed_x = frame.x;
  st.delayed_p = frame.p;
  st.delayed_ex = frame.ex;
  st.delayed_ep = frame.ep;
  st.delayed_exp = frame.exp;
  vad.unwrap_or(0.0)
}

/// The voice probability `process_frame` would return, without the pitch filter, gains and
/// synthesis. The state they skip (`lastg`, `delayed_*`, `synthesis_mem`) never feeds the
/// analysis or the network, so the network state and the VAD sequence evolve exactly as there.
pub(crate) fn voice_probability(
  model: &RnnModel,
  tables: &Tables,
  fft: &mut Transform,
  st: &mut Memory,
  input: &[f32; FRAME_SIZE],
) -> f32 {
  let mut frame = Frame::new();
  let mut g = [0.0f32; NB_BANDS];
  analyze(model, tables, fft, st, &mut frame, &mut g, input).unwrap_or(0.0)
}

/// HP filter, frame analysis and, unless the frame is silent, the network (gains into `g`).
/// Returns the voice probability, `None` for a silent frame.
fn analyze(
  model: &RnnModel,
  tables: &Tables,
  fft: &mut Transform,
  st: &mut Memory,
  frame: &mut Frame,
  g: &mut [f32; NB_BANDS],
  input: &[f32; FRAME_SIZE],
) -> Option<f32> {
  let mut x = [0.0f32; FRAME_SIZE];
  biquad(&mut x, &mut st.mem_hp_x, input);
  let silence = compute_frame_features(tables, fft, st, frame, &x);
  (!silence).then(|| model.compute(&mut st.rnn, g, &frame.features))
}

/// `rnn_compute_frame_features`; returns true for a silent frame (features cleared).
fn compute_frame_features(
  tables: &Tables,
  fft: &mut Transform,
  st: &mut Memory,
  f: &mut Frame,
  x: &[f32; FRAME_SIZE],
) -> bool {
  // rnn_frame_analysis: [previous frame | this frame], windowed.
  let mut win = [0.0f32; WINDOW_SIZE];
  win.iter_mut().zip(st.analysis_mem.iter().chain(x)).for_each(|(w, v)| *w = *v);
  st.analysis_mem = *x;
  tables.apply_window(&mut win);
  fft.forward(&mut f.x, &win);
  f.ex = band_energy(&f.x);

  st.pitch_buf.copy_within(FRAME_SIZE.., 0);
  st.pitch_buf.iter_mut().skip(PITCH_BUF_SIZE - FRAME_SIZE).zip(x).for_each(|(b, v)| *b = *v);
  let pitch_index = st.pitch.find(&st.pitch_buf);
  // The window one pitch period back; `find` keeps the period below PITCH_MAX_PERIOD.
  let start = PITCH_BUF_SIZE - WINDOW_SIZE - pitch_index;
  win.iter_mut().zip(st.pitch_buf.iter().skip(start)).for_each(|(w, v)| *w = *v);
  tables.apply_window(&mut win);
  fft.forward(&mut f.p, &win);
  f.ep = band_energy(&f.p);
  f.exp = band_corr(&f.x, &f.p);
  for ((exp, &ex), &ep) in f.exp.iter_mut().zip(&f.ex).zip(&f.ep) {
    *exp = (*exp as f64 / (0.001 + (ex * ep) as f64).sqrt()) as f32;
  }
  let (cepstrum, rest) = f.features.split_at_mut(NB_BANDS);
  tables.dct(rest, &f.exp);
  if let Some(last) = rest.last_mut() {
    *last = (0.01 * (pitch_index as f64 - 300.0)) as f32;
  }

  let mut ly = [0.0f32; NB_BANDS];
  let (mut log_max, mut follow, mut e) = (-2.0f32, -2.0f32, 0.0f32);
  // Mixed f32/f64 as in the C: `follow - 1.5` and the MAX16 comparisons are double.
  let max = |a: f64, b: f64| if a > b { a } else { b };
  for (ly, &ex) in ly.iter_mut().zip(&f.ex) {
    let v = (1e-2 + ex as f64).log10() as f32;
    let v = max((log_max - 7.0) as f64, max(follow as f64 - 1.5, v as f64)) as f32;
    *ly = v;
    log_max = if log_max > v { log_max } else { v };
    follow = max(follow as f64 - 1.5, v as f64) as f32;
    e += ex;
  }
  if (e as f64) < SILENCE_ENERGY {
    // No audio: leave the network state alone.
    f.features.fill(0.0);
    return true;
  }
  tables.dct(cepstrum, &ly);
  if let [c0, c1, ..] = cepstrum {
    *c0 -= 12.0;
    *c1 -= 4.0;
  }
  false
}

/// `frame_synthesis`: inverse FFT of `delayed_x`, window, overlap-add.
fn frame_synthesis(tables: &Tables, fft: &mut Transform, st: &mut Memory, out: &mut [f32; FRAME_SIZE]) {
  let mut x = [0.0f32; WINDOW_SIZE];
  fft.inverse(&mut x, &st.delayed_x);
  tables.apply_window(&mut x);
  let (first, second) = x.split_at(FRAME_SIZE);
  for ((o, &x), &m) in out.iter_mut().zip(first).zip(&st.synthesis_mem) {
    *o = x + m;
  }
  st.synthesis_mem.iter_mut().zip(second).for_each(|(m, &x)| *m = x);
}

/// `rnn_biquad`: `y = x + mem0` in f32, the memory update in f64 with f32 coefficients.
fn biquad(y: &mut [f32; FRAME_SIZE], mem: &mut [f32; 2], x: &[f32; FRAME_SIZE]) {
  let [b0, b1] = B_HP.map(f64::from);
  let [a0, a1] = A_HP.map(f64::from);
  for (y, &xi) in y.iter_mut().zip(x) {
    let yi = xi + mem[0];
    mem[0] = (mem[1] as f64 + (b0 * xi as f64 - a0 * yi as f64)) as f32;
    mem[1] = (b1 * xi as f64 - a1 * yi as f64) as f32;
    *y = yi;
  }
}

/// `rnn_pitch_filter`: adds the pitch-lagged spectrum where the network's gain says the band is
/// more periodic than the correlation shows, then renormalizes each band to its old energy.
fn pitch_filter(
  x: &mut [Complex32; FREQ_SIZE],
  p: &[Complex32; FREQ_SIZE],
  ex: &[f32; NB_BANDS],
  ep: &[f32; NB_BANDS],
  exp: &[f32; NB_BANDS],
  g: &[f32; NB_BANDS],
) {
  let mut r = [0.0f32; NB_BANDS];
  for ((((r, &exp), &g), &ex), &ep) in r.iter_mut().zip(exp).zip(g).zip(ex).zip(ep) {
    let v = if exp > g {
      1.0
    } else {
      let num = (exp * exp) * (1.0 - g * g);
      (num as f64 / (0.001 + ((g * g) * (1.0 - exp * exp)) as f64)) as f32
    };
    // sqrt(MIN16(1, MAX16(0, r))), NaN passes through like the C macros.
    let v = if 0.0 > v { 0.0 } else { v };
    let v = if 1.0 < v { 1.0 } else { v };
    let v = (v as f64).sqrt() as f32;
    *r = (v as f64 * (ex as f64 / (1e-8 + ep as f64)).sqrt()) as f32;
  }
  let mut rf = [0.0f32; FREQ_SIZE];
  interp_band_gain(&mut rf, &r);
  for ((x, p), &rf) in x.iter_mut().zip(p).zip(&rf) {
    x.re += rf * p.re;
    x.im += rf * p.im;
  }
  let new_e = band_energy(x);
  let mut norm = [0.0f32; NB_BANDS];
  for ((n, &ex), &ne) in norm.iter_mut().zip(ex).zip(&new_e) {
    *n = (ex as f64 / (1e-8 + ne as f64)).sqrt() as f32;
  }
  let mut normf = [0.0f32; FREQ_SIZE];
  interp_band_gain(&mut normf, &norm);
  for (x, &n) in x.iter_mut().zip(&normf) {
    x.re *= n;
    x.im *= n;
  }
}
