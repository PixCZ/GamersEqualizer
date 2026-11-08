//! DPDFNet-2 48 kHz (Ceva-IP, Apache-2.0): DeepFilterNet2 with dual-path RNN blocks. Better on
//! soft speech under keyboard noise than DeepFilterNet 3, at ~3x its CPU (~11 % of a core).
//! Port of upstream's `dpdfnet.StreamEnhancer`: the ONNX graph takes one spectrum frame plus a
//! flat state vector (feature normalisation + RNN states); the Vorbis STFT (960 / 480) and the
//! overlap-add run here. Runs on tract 0.23, which is ~3x faster than the 0.21 DeepFilterNet 3
//! is pinned to; both versions are linked side by side.

use std::collections::VecDeque;
use std::f32::consts::PI;
use std::io::Cursor;
use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use tract_onnx_23::prelude::*;
use tract_onnx_23::tract_core::internal::{bail, format_err};

/// The model file, embedded so the APO (inside audiodg) never reads from disk.
static MODEL_ONNX: &[u8] = include_bytes!("../models/dpdfnet2_48khz_hr.onnx");
const WIN: usize = 960;
pub const HOP: usize = 480;
const BINS: usize = WIN / 2 + 1;
/// The enhanced spectrum of frame t belongs to input frame t - 4 (measured: total delay
/// 2880 samples = 480 priming + 480 STFT overlap + 4 hops). The dry signal for the strength
/// blend is delayed by the same 4 hops, else it would comb-filter.
/// The STFT overlap is the HOP zeros `in_buf` starts with, so a frame runs as soon as one hop
/// arrives, like DeepFilterNet 3. Waiting for a full window instead made the delay depend on
/// the block size (2400 at 480-frame blocks, 2880 at 441), and the gate trusts the constant.
const LOOKAHEAD_HOPS: usize = 4;
/// Added latency, 48 kHz samples (checked in `test_measured_latency_matches_constants`).
pub const LATENCY_SAMPLES: usize = HOP + (WIN - HOP) + LOOKAHEAD_HOPS * HOP;

pub struct DpdfEngine {
  /// One run state for the engine's lifetime: `plan.run()` would build a fresh one (and
  /// allocate every intermediate buffer) per 10 ms frame.
  runner: TypedSimpleState,
  init_state: Vec<f32>,
  state: Tensor,
  fft: Arc<dyn Fft<f32>>,
  ifft: Arc<dyn Fft<f32>>,
  scratch: Vec<Complex32>,
  window: Vec<f32>,
  spec: Vec<Complex32>,
  /// Sliding analysis window (needs WIN samples before a frame runs).
  in_buf: Vec<f32>,
  overlap: Vec<f32>,
  /// Input delayed by LOOKAHEAD_HOPS hops, aligned with the enhanced output.
  dry: VecDeque<f32>,
  output_fifo: Vec<f32>,
  error_logged: bool,
}

impl DpdfEngine {
  /// Parses and optimises the embedded model (~120 ms).
  pub fn load() -> Result<Self, String> {
    Self::try_load().map_err(|e| format!("{e:#}"))
  }

  /// Process samples in place; `atten_lim` = share of the dry signal mixed back in (None = full
  /// suppression). Output length == input length (one primed hop absorbs any block size).
  pub fn process(&mut self, samples: &mut [f32], atten_lim: Option<f32>) {
    self.in_buf.extend_from_slice(samples);
    let mut hop = [0.0f32; HOP];
    while self.in_buf.len() >= WIN {
      let ok = match self.run_frame(&mut hop) {
        Ok(()) => true,
        Err(e) => {
          if !self.error_logged {
            log::error!("[Denoise] DPDFNet frame failed, passing audio through: {e:#}");
            self.error_logged = true;
          }
          false
        }
      };
      for (wet, x) in hop.iter().zip(self.in_buf.iter().take(HOP)) {
        self.dry.push_back(*x);
        let dry = self.dry.pop_front().unwrap_or(0.0);
        self.output_fifo.push(match (ok, atten_lim) {
          (false, _) => dry,
          (true, Some(lim)) => wet * (1.0 - lim) + dry * lim,
          (true, None) => *wet,
        });
      }
      self.in_buf.drain(..HOP);
    }
    crate::ai_denoiser::drain_fifo(&mut self.output_fifo, samples);
  }

  /// Cold start: the recurrent state back to `init_state`, written into the current tensor (no
  /// allocation, so a pause flush can use it), and fresh buffers.
  pub fn reset(&mut self) {
    let cleared = match self.state.try_as_plain_ram_mut() {
      Ok(mut ram) => match ram.as_slice_mut::<f32>() {
        Ok(s) if s.len() == self.init_state.len() => {
          s.copy_from_slice(&self.init_state);
          true
        }
        _ => false,
      },
      Err(_) => false,
    };
    if !cleared {
      self.state = tensor1(&self.init_state); // not a plain f32 state of that size: rebuild it
    }
    self.restart();
  }

  /// Fresh buffers, but the recurrent state kept: after a pause the model resumes warm (a cold
  /// state over-suppresses the first ~100 ms of speech).
  pub fn restart(&mut self) {
    self.in_buf.clear();
    self.in_buf.resize(WIN - HOP, 0.0);
    self.overlap.fill(0.0);
    self.dry.clear();
    self.dry.resize(LOOKAHEAD_HOPS * HOP, 0.0);
    crate::ai_denoiser::reprime_fifo(&mut self.output_fifo, HOP);
  }

  fn try_load() -> TractResult<Self> {
    let onnx = tract_onnx_23::onnx();
    let proto = onnx.proto_model_for_read(&mut Cursor::new(MODEL_ONNX))?;
    let meta = |key: &str| proto.metadata_props.iter().find(|p| p.key == key).map(|p| p.value.as_str());
    if let Some(sr) = meta("sample_rate") {
      if sr != "48000" {
        bail!("unexpected model sample rate {sr}");
      }
    }
    let floats = |key: &str| -> TractResult<Vec<f32>> {
      meta(key)
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().parse::<f32>().map_err(|e| format_err!("metadata {key}: {e}")))
        .collect()
    };
    let size: usize = meta("state_size").ok_or_else(|| format_err!("metadata state_size missing"))?.parse()?;
    let (erb, spec) = (floats("erb_norm_init")?, floats("spec_norm_init")?);
    if erb.len() + spec.len() > size {
      bail!("norm init ({} + {}) exceeds state size {size}", erb.len(), spec.len());
    }
    // Layout from upstream: [erb_norm | spec_norm | RNN states (zeros)].
    let init_state: Vec<f32> = erb.iter().chain(&spec).copied().chain(std::iter::repeat_n(0.0, size - erb.len() - spec.len())).collect();

    let plan = onnx
      .model_for_proto_model(&proto)?
      .with_input_fact(0, f32::fact([1, 1, BINS, 2]).into())?
      .with_input_fact(1, f32::fact([size]).into())?
      .into_optimized()?
      .into_runnable()?;

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(WIN);
    let ifft = planner.plan_fft_inverse(WIN);
    let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len().max(ifft.get_inplace_scratch_len())];
    // Vorbis window (upstream `vorbis_window`); its square is power-complementary at 50 %.
    let window = (0..WIN)
      .map(|i| {
        let s = (0.5 * PI * (i as f32 + 0.5) / (WIN as f32 / 2.0)).sin();
        (0.5 * PI * s * s).sin()
      })
      .collect();
    let mut engine = Self {
      runner: plan.spawn()?,
      state: tensor1(&init_state),
      init_state,
      fft,
      ifft,
      scratch,
      window,
      spec: vec![Complex32::default(); WIN],
      in_buf: Vec::with_capacity(WIN + HOP * 4),
      overlap: vec![0.0; WIN],
      dry: VecDeque::new(),
      output_fifo: Vec::new(),
      error_logged: false,
    };
    engine.reset();
    Ok(engine)
  }

  /// One model step on `in_buf[..WIN]`; writes the next HOP finished samples.
  fn run_frame(&mut self, hop: &mut [f32; HOP]) -> TractResult<()> {
    for ((c, x), w) in self.spec.iter_mut().zip(&self.in_buf).zip(&self.window) {
      *c = Complex32::new(x * w, 0.0);
    }
    self.fft.process_with_scratch(&mut self.spec, &mut self.scratch);

    let mut input = Tensor::zero::<f32>(&[1, 1, BINS, 2])?;
    for ([re, im], c) in input.try_as_plain_ram_mut()?.as_slice_mut::<f32>()?.as_chunks_mut::<2>().0.iter_mut().zip(&self.spec) {
      *re = c.re;
      *im = c.im;
    }
    let state = std::mem::replace(&mut self.state, tensor0(0f32));
    let outputs = match self.runner.run(tvec![input.into(), state.into()]) {
      Ok(o) => o,
      Err(e) => {
        self.state = tensor1(&self.init_state); // the state moved into the failed run
        return Err(e);
      }
    };
    let mut outputs = outputs.into_iter();
    let (Some(spec_e), Some(state_out)) = (outputs.next(), outputs.next()) else {
      self.state = tensor1(&self.init_state);
      bail!("model returned fewer than 2 outputs");
    };
    self.state = state_out.into_tensor();

    // Inverse real FFT: the model's 481 bins, then the conjugate-symmetric upper half.
    let spec_e = spec_e.into_tensor();
    let e = spec_e.try_as_plain_ram()?;
    let e = e.as_slice::<f32>()?;
    let (lo, hi) = self.spec.split_at_mut(BINS);
    for (c, [re, im]) in lo.iter_mut().zip(e.as_chunks::<2>().0) {
      *c = Complex32::new(*re, *im);
    }
    for (c, mirror) in hi.iter_mut().zip(lo.iter().skip(1).take(WIN - BINS).rev()) {
      *c = mirror.conj();
    }
    self.ifft.process_with_scratch(&mut self.spec, &mut self.scratch);
    let scale = 1.0 / WIN as f32;
    for ((o, c), w) in self.overlap.iter_mut().zip(&self.spec).zip(&self.window) {
      *o += c.re * scale * w;
    }
    for (h, o) in hop.iter_mut().zip(&self.overlap) {
      *h = *o;
    }
    if !hop.iter().all(|h| h.is_finite()) {
      // A NaN would live on in the recurrent state forever: start the model over.
      self.state = tensor1(&self.init_state);
      self.overlap.fill(0.0);
      bail!("model produced non-finite output");
    }
    self.overlap.copy_within(HOP.., 0);
    for o in self.overlap.iter_mut().skip(WIN - HOP) {
      *o = 0.0;
    }
    Ok(())
  }
}
