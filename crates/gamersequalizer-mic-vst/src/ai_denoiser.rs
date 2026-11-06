//! Neural noise suppression: RNNoise (nnnoiseless, light), DeepFilterNet 3 (higher quality,
//! ~4 % of a core) or DPDFNet-2 (best on soft speech, ~11 %, see `dpdfnet`). The heavy models
//! load on first selection (DFN3 ~200 ms, DPDFNet ~120 ms) and then stay in memory. Its runtime is not `Send` (tract uses `Rc`), so it can't be loaded
//! on another thread: the stream selects the model before audio starts (no gap); switching
//! live costs one ~0.2 s gap the first time. Upgrade path: a dedicated DF worker thread.

use crate::dpdfnet::{self, DpdfEngine};
use df::tract::{DfParams, DfTract, RuntimeParams};
use gamersequalizer_common::shm::{DenoiseLimit, DenoiseModel};
use ndarray::Array2;
use nnnoiseless::{DenoiseState, FRAME_SIZE};

/// nnnoiseless (RNNoise) is trained on i16-range samples, not [-1.0, 1.0].
const I16_SCALE: f32 = 32768.0;
/// Attenuation limit of the heavy models at strength 0+ .. just below 1.0 (100 % = unlimited).
const DF_MIN_ATTEN_DB: f32 = 6.0;
const DF_MAX_ATTEN_DB: f32 = 60.0;
/// Below this local SNR DeepFilterNet treats a frame as noise only and zeroes it. Upstream's
/// -10 dB dropped 16-26 % of soft syllables under loud mechanical keys; -20 dB drops 3-8 %
/// with the same key suppression (`test_deepfilter_keeps_soft_speech_over_keyboard`).
const DF_MIN_DB_THRESH: f32 = -20.0;
/// Upstream defaults for the two upper stages (skip DF / skip all processing on clean speech).
const DF_MAX_DB_ERB_THRESH: f32 = 30.0;
const DF_MAX_DB_DF_THRESH: f32 = 20.0;
/// A frame is speech above this RNNoise voice probability. Measured on mechanical keys + mouse
/// clicks without speech: never fired at 0.5 (DeepFilterNet's local SNR fired on 9-31 %).
const VOICE_PROB: f32 = 0.5;
/// ...for this many 10 ms frames in a row: a key click is shorter than a syllable, and very
/// loud keys (-20 dBFS) occasionally score one speech-like frame.
const VOICE_MIN_FRAMES: u32 = 2;
/// Once speech was detected, frames above this much weaker probability keep the gate open
/// (soft syllables, word tails), as long as speech was heard within `VOICE_ACTIVE_FRAMES`.
/// Measured (`gate_sweep`): soft syllables cut by the gate under loud keys 5.3 -> 0.6 % (RNNoise),
/// 3.5 -> 0.6 % over no gate (DFN3); keys alone score above it on 2-8 % of frames, a fan on
/// 0.4-1.6 %, and typing right after speech keeps the gate open +0.2 % longer.
const VOICE_KEEP_PROB: f32 = 0.05;
const VOICE_ACTIVE_FRAMES: u32 = 20;
/// The second detector, on the denoised output, opens on one frame above this and stays open
/// longer: keys are mostly gone there, and a cut voice is worse than a few keys let through.
/// Measured on a private voice under its own keyboard (`private_report`): phrase onsets after
/// a pause scored 0.6-0.9 there for one frame but < 0.1 on the raw input; soft syllables cut
/// by the gate over no gate 6-15 -> 1-4 %. Cost: typing alone (ESC-50 keys, -20 dBFS) keeps
/// it open most of the time with RNNoise/DFN3, so only the floor below stops those keys.
const OUT_VOICE_PROB: f32 = 0.3;
const OUT_VOICE_MIN_FRAMES: u32 = 1;
const OUT_VOICE_KEEP_PROB: f32 = 0.02;
const OUT_VOICE_ACTIVE_FRAMES: u32 = 50;
/// A closed gate attenuates by this much instead of muting, so a late open only makes a word
/// onset quieter, never cuts it (lost soft syllables, own voice: up to 1.6 percentage points
/// less than muting). Keys between words come through 12 dB under the denoiser's output.
const GATE_FLOOR_DB: f32 = -12.0;
/// Shared-memory gate value at or below this = gate off; anything above = automatic gate.
/// (The setting used to be a level threshold; the gate no longer has one.)
const GATE_OFF_DB: f32 = -79.5;
/// Gate stays open this long after the last speech frame (word tails, short pauses), counted
/// at the output: the detector hears the raw input a model latency early, so the gate adds
/// that latency on top (else DPDFNet's 60 ms lead would leave only ~140 ms).
const GATE_HOLD_SAMPLES: usize = 48 * 200;
/// Per-sample smoothing of the gate gain: ~1 ms to open, ~50 ms to close (no clicks).
const GATE_OPEN_COEF: f32 = 1.0 / 48.0;
const GATE_CLOSE_COEF: f32 = 1.0 / (48.0 * 50.0);
/// Added latency, 48 kHz samples. Both = output FIFO priming (one frame, so any block size
/// works) + the model's own delay. Measured in `test_measured_latency_matches_constants`.
/// RNNoise: 480 priming + 480 overlap-add window.
const RNNOISE_LATENCY_SAMPLES: usize = 2 * FRAME_SIZE;
/// DeepFilterNet 3: 480 priming + (fft 960 - hop 480) STFT overlap + 2 lookahead frames * 480.
const DF_LATENCY_SAMPLES: usize = 4 * 480;
/// DPDFNet: 480 priming + 480 STFT overlap + 4 lookahead hops (2880 = 60 ms).
const DPDF_LATENCY_SAMPLES: usize = dpdfnet::LATENCY_SAMPLES;

pub struct AiVoiceIsolator {
  rnnoise: RnNoiseEngine,
  deepfilter: Option<Box<DeepFilterEngine>>,
  df_error: Option<String>,
  dpdfnet: Option<Box<DpdfEngine>>,
  dpdf_error: Option<String>,
  /// The selected heavy model was too slow for real time on this CPU; RNNoise runs until a
  /// model is selected again.
  overloaded: bool,
  selected: DenoiseModel,
  active: DenoiseModel,
  strength: f32, // 0.0 to 1.0
  gate: NoiseGate,
  vad: VoiceDetector,
  /// Same detector on the denoised output: keys are gone there, so it keeps hearing a voice the
  /// raw-input one loses under typing (private voice, keys at -10 dB SNR: RNNoise's probability
  /// on soft syllables fell from ~0.95 to <0.1 and the gate cut 18-31 % of them).
  vad_out: VoiceDetector,
  bypassed: bool,
}

struct RnNoiseEngine {
  denoiser: Box<DenoiseState<'static>>,
  input_fifo: Vec<f32>,
  output_fifo: Vec<f32>,
  /// Previous dry frame: RNNoise output lags its input by one frame, so the dry
  /// signal for the strength blend must be delayed by the same amount (else comb filtering).
  prev_dry: [f32; FRAME_SIZE],
}

struct DeepFilterEngine {
  model: DfTract,
  hop: usize,
  input_fifo: Vec<f32>,
  output_fifo: Vec<f32>,
  in_frame: Array2<f32>,
  out_frame: Array2<f32>,
  error_logged: bool,
}

/// RNNoise's voice probability; drives the gate for all models. One instance hears the raw
/// microphone, ahead of the denoised output (DeepFilterNet 40 ms, RNNoise 20 ms), so word
/// onsets open the gate in time; a second hears the denoised output (`OUT_VOICE_PROB`). Costs one extra RNNoise pass (~1 % of a core) even when RNNoise is the denoiser:
/// simpler than sharing its VAD; share it if CPU ever matters.
struct VoiceDetector {
  state: Box<DenoiseState<'static>>,
  fifo: Vec<f32>,
  frame: [f32; FRAME_SIZE],
  scratch: [f32; FRAME_SIZE],
  /// Consecutive speech frames so far.
  run: u32,
  /// Frames left in which a weaker frame (above `keep_prob`) still counts as speech.
  active_left: u32,
  keep_prob: f32,
  active_frames: u32,
  open_prob: f32,
  min_frames: u32,
}

/// Automatic speech gate: open while a detector hears speech, plus a hold; fades instead of
/// chopping, and closed only attenuates (`GATE_FLOOR_DB`). No level threshold, so it works the
/// same for a loud or a quiet voice (day vs. evening). Classic attack/hold/release keyed by a VAD.
/// Tuned to never cut a voice; some loud keys get through (see `OUT_VOICE_PROB`).
struct NoiseGate {
  enabled: bool,
  hold_left: usize,
  gain: f32,
  floor: f32,
}

impl Default for AiVoiceIsolator {
  fn default() -> Self {
    Self::new()
  }
}

impl AiVoiceIsolator {
  pub fn new() -> Self {
    Self {
      rnnoise: RnNoiseEngine::new(),
      deepfilter: None,
      df_error: None,
      dpdfnet: None,
      dpdf_error: None,
      overloaded: false,
      selected: DenoiseModel::RnNoise,
      active: DenoiseModel::RnNoise,
      strength: 0.75,
      gate: NoiseGate::new(true),
      vad: VoiceDetector::new(),
      vad_out: VoiceDetector::new_output(),
      bypassed: false,
    }
  }

  pub fn set_strength(&mut self, strength: f32) {
    self.strength = strength.clamp(0.0, 1.0);
  }

  /// Shared-memory gate setting: at or below `GATE_OFF_DB` = off, anything else = automatic.
  pub fn set_gate_threshold_db(&mut self, db: f32) {
    self.gate.enabled = db > GATE_OFF_DB;
  }

  /// Choose the model. A heavy model is loaded on first request (blocking, 0.1-0.2 s); if it
  /// fails, RNNoise keeps running and the error is kept for the UI/log.
  pub fn set_model(&mut self, model: DenoiseModel) {
    if model != self.selected {
      self.overloaded = false; // picking a model again = try it again
    }
    self.selected = model;
    match model {
      DenoiseModel::DeepFilterNet if self.deepfilter.is_none() && self.df_error.is_none() => {
        load_into(&mut self.deepfilter, &mut self.df_error, "DeepFilterNet 3", DeepFilterEngine::load)
      }
      DenoiseModel::DpdfNet if self.dpdfnet.is_none() && self.dpdf_error.is_none() => {
        load_into(&mut self.dpdfnet, &mut self.dpdf_error, "DPDFNet", DpdfEngine::load)
      }
      _ => {}
    }
  }

  pub fn selected_model(&self) -> DenoiseModel {
    self.selected
  }

  /// Give up on the heavy model for now (the caller saw it miss the real-time deadline).
  pub fn fall_back_overloaded(&mut self) {
    if self.active.is_heavy() {
      log::warn!("[Denoise] {:?} too slow for real time on this CPU, switching to RNNoise", self.active);
      self.overloaded = true;
    }
  }

  /// Why the selected model is not running (None if it is, or RNNoise is selected).
  pub fn limit(&self) -> DenoiseLimit {
    let load_error = match self.selected {
      DenoiseModel::DeepFilterNet => self.df_error.is_some(),
      DenoiseModel::DpdfNet => self.dpdf_error.is_some(),
      _ => return DenoiseLimit::None,
    };
    if load_error {
      DenoiseLimit::LoadFailed
    } else if self.overloaded {
      DenoiseLimit::Overloaded
    } else {
      DenoiseLimit::None
    }
  }

  /// Delay the model adds to the microphone, in ms (0 for Unknown).
  pub fn latency_ms(model: DenoiseModel) -> f32 {
    latency_samples(model) as f32 / 48.0
  }

  /// Model that is actually processing audio right now.
  pub fn active_model(&self) -> DenoiseModel {
    self.active
  }

  /// Process microphone samples in place.
  /// RNNoise adds a fixed latency of one frame (10 ms) plus the model's own frame;
  /// every output sample is processed, whatever the block size.
  pub fn process_samples(&mut self, samples: &mut [f32]) {
    let loaded = match self.selected {
      DenoiseModel::DeepFilterNet => self.deepfilter.is_some(),
      DenoiseModel::DpdfNet => self.dpdfnet.is_some(),
      _ => false,
    };
    let wanted = if loaded && !self.overloaded { self.selected } else { DenoiseModel::RnNoise };
    if wanted != self.active {
      // Fresh state for the engine taking over; its FIFOs may hold stale audio.
      self.reset_engines();
      self.active = wanted;
    }

    if self.strength <= 0.001 {
      self.bypassed = true;
      return;
    }
    if self.bypassed {
      // Stale FIFO content from before the bypass would replay old audio.
      self.reset();
    }

    let voice = self.vad.process(samples); // raw input, before it is denoised in place
    match (self.active, self.deepfilter.as_mut(), self.dpdfnet.as_mut()) {
      (DenoiseModel::DeepFilterNet, Some(df), _) => df.process(samples, self.strength),
      (DenoiseModel::DpdfNet, _, Some(dp)) => dp.process(samples, atten_lim(self.strength)),
      _ => self.rnnoise.process(samples, self.strength),
    }
    // The input detector runs a model latency ahead of the output (opens on word onsets in
    // time); the output one is in step with it, so its hold needs no latency on top.
    let voice_out = self.gate.enabled && self.vad_out.process(samples);
    let hold = match (voice, voice_out) {
      (true, _) => GATE_HOLD_SAMPLES + latency_samples(self.active),
      (false, true) => GATE_HOLD_SAMPLES,
      (false, false) => 0,
    };
    self.gate.process(samples, hold);
  }

  pub fn reset(&mut self) {
    self.reset_engines();
    self.gate = NoiseGate::new(self.gate.enabled);
    self.vad = VoiceDetector::new();
    self.vad_out = VoiceDetector::new_output();
    self.bypassed = false;
  }

  fn reset_engines(&mut self) {
    self.rnnoise.reset();
    if let Some(df) = self.deepfilter.as_mut() {
      df.reset();
    }
    if let Some(dp) = self.dpdfnet.as_mut() {
      dp.reset();
    }
  }
}

/// Loads a heavy engine once, logging the outcome; the error is kept so it is not retried.
fn load_into<E>(
  slot: &mut Option<Box<E>>,
  error: &mut Option<String>,
  name: &str,
  load: impl FnOnce() -> Result<E, String>,
) {
  let t0 = std::time::Instant::now();
  match load() {
    Ok(engine) => {
      log::info!("[Denoise] {name} model loaded in {:?}", t0.elapsed());
      *slot = Some(Box::new(engine));
    }
    Err(e) => {
      log::error!("[Denoise] {name} failed to load, staying on RNNoise: {e}");
      *error = Some(e);
    }
  }
}

/// Strength -> share of the dry signal the heavy models mix back (None = unlimited).
fn atten_lim(strength: f32) -> Option<f32> {
  if strength >= 0.999 {
    None
  } else {
    let db = DF_MIN_ATTEN_DB + strength * (DF_MAX_ATTEN_DB - DF_MIN_ATTEN_DB);
    Some(10f32.powf(-db / 20.0))
  }
}

impl RnNoiseEngine {
  fn new() -> Self {
    Self {
      denoiser: DenoiseState::new(),
      input_fifo: Vec::with_capacity(FRAME_SIZE * 4),
      output_fifo: primed_fifo(FRAME_SIZE),
      prev_dry: [0.0; FRAME_SIZE],
    }
  }

  fn process(&mut self, samples: &mut [f32], strength: f32) {
    self.input_fifo.extend_from_slice(samples);

    let mut temp_out = [0.0f32; FRAME_SIZE];
    let mut temp_in = [0.0f32; FRAME_SIZE];
    let alpha = strength;

    while self.input_fifo.len() >= FRAME_SIZE {
      for (dst, src) in temp_in.iter_mut().zip(self.input_fifo.drain(..FRAME_SIZE)) {
        *dst = src * I16_SCALE;
      }

      self.denoiser.process_frame(&mut temp_out, &temp_in);

      for (wet, dry) in temp_out.iter().zip(self.prev_dry.iter()) {
        self.output_fifo.push(alpha * (wet / I16_SCALE) + (1.0 - alpha) * (dry / I16_SCALE));
      }
      self.prev_dry = temp_in;
    }

    drain_fifo(&mut self.output_fifo, samples);
  }

  fn reset(&mut self) {
    self.denoiser = DenoiseState::new();
    self.input_fifo.clear();
    self.output_fifo = primed_fifo(FRAME_SIZE);
    self.prev_dry = [0.0; FRAME_SIZE];
  }
}

impl DeepFilterEngine {
  fn load() -> Result<Self, String> {
    let params =
      RuntimeParams::default_with_ch(1).with_thresholds(DF_MIN_DB_THRESH, DF_MAX_DB_ERB_THRESH, DF_MAX_DB_DF_THRESH);
    let model = DfTract::new(DfParams::default(), &params).map_err(|e| format!("{e:#}"))?;
    let hop = model.hop_size.max(1);
    if model.sr != 48_000 {
      return Err(format!("unexpected model sample rate {}", model.sr));
    }
    Ok(Self {
      model,
      hop,
      input_fifo: Vec::with_capacity(hop * 4),
      output_fifo: primed_fifo(hop),
      in_frame: Array2::zeros((1, hop)),
      out_frame: Array2::zeros((1, hop)),
      error_logged: false,
    })
  }

  fn process(&mut self, samples: &mut [f32], strength: f32) {
    // Strength maps to the model's own attenuation limit (no dry/wet blend: DF's output
    // is delayed by its look-ahead, a blend would comb-filter).
    self.model.atten_lim = atten_lim(strength);

    self.input_fifo.extend_from_slice(samples);
    while self.input_fifo.len() >= self.hop {
      for (dst, src) in self.in_frame.iter_mut().zip(self.input_fifo.drain(..self.hop)) {
        *dst = src;
      }
      match self.model.process(self.in_frame.view(), self.out_frame.view_mut()) {
        Ok(_) => self.output_fifo.extend(self.out_frame.iter().copied()),
        Err(e) => {
          if !self.error_logged {
            log::error!("[Denoise] DeepFilterNet frame failed, passing audio through: {e:#}");
            self.error_logged = true;
          }
          self.output_fifo.extend(self.in_frame.iter().copied());
        }
      }
    }
    drain_fifo(&mut self.output_fifo, samples);
  }

  fn reset(&mut self) {
    self.input_fifo.clear();
    self.output_fifo = primed_fifo(self.hop);
  }
}

impl VoiceDetector {
  fn new() -> Self {
    Self {
      state: DenoiseState::new(),
      fifo: Vec::with_capacity(FRAME_SIZE * 4),
      frame: [0.0; FRAME_SIZE],
      scratch: [0.0; FRAME_SIZE],
      run: 0,
      active_left: 0,
      keep_prob: VOICE_KEEP_PROB,
      active_frames: VOICE_ACTIVE_FRAMES,
      open_prob: VOICE_PROB,
      min_frames: VOICE_MIN_FRAMES,
    }
  }

  /// Detector for the denoised output, see `OUT_VOICE_PROB`.
  fn new_output() -> Self {
    Self {
      open_prob: OUT_VOICE_PROB,
      min_frames: OUT_VOICE_MIN_FRAMES,
      keep_prob: OUT_VOICE_KEEP_PROB,
      active_frames: OUT_VOICE_ACTIVE_FRAMES,
      ..Self::new()
    }
  }

  /// True if any frame completed in this block is speech.
  fn process(&mut self, samples: &[f32]) -> bool {
    self.fifo.extend_from_slice(samples);
    let mut voice = false;
    while self.fifo.len() >= FRAME_SIZE {
      for (dst, src) in self.frame.iter_mut().zip(self.fifo.drain(..FRAME_SIZE)) {
        *dst = src * I16_SCALE;
      }
      let prob = self.state.process_frame(&mut self.scratch, &self.frame);
      self.run = if prob > self.open_prob { self.run.saturating_add(1) } else { 0 };
      // Hysteresis: opening needs a clear syllable; once talking, a soft one keeps it open.
      let frame_voice = self.run >= self.min_frames || (self.active_left > 0 && prob > self.keep_prob);
      self.active_left = if frame_voice { self.active_frames } else { self.active_left.saturating_sub(1) };
      voice |= frame_voice;
    }
    voice
  }
}

impl NoiseGate {
  fn new(enabled: bool) -> Self {
    Self { enabled, hold_left: 0, gain: 0.0, floor: 10f32.powf(GATE_FLOOR_DB / 20.0) }
  }

  /// `hold`: samples to stay open from now (speech heard in this block), 0 = none heard.
  /// Never shortens a running hold.
  fn process(&mut self, samples: &mut [f32], hold: usize) {
    if !self.enabled {
      return;
    }
    self.hold_left = self.hold_left.max(hold);
    for s in samples.iter_mut() {
      self.hold_left = self.hold_left.saturating_sub(1);
      let (target, coef) = if self.hold_left > 0 { (1.0, GATE_OPEN_COEF) } else { (self.floor, GATE_CLOSE_COEF) };
      self.gain += (target - self.gain) * coef;
      *s *= self.gain;
    }
  }
}

fn latency_samples(model: DenoiseModel) -> usize {
  match model {
    DenoiseModel::RnNoise => RNNOISE_LATENCY_SAMPLES,
    DenoiseModel::DeepFilterNet => DF_LATENCY_SAMPLES,
    DenoiseModel::DpdfNet => DPDF_LATENCY_SAMPLES,
    DenoiseModel::Unknown(_) => 0,
  }
}

/// One frame of silence keeps output length == input length for any block size.
pub(crate) fn primed_fifo(frame: usize) -> Vec<f32> {
  let mut fifo = Vec::with_capacity(frame * 4);
  fifo.resize(frame, 0.0);
  fifo
}

/// Move processed samples out; the primed frame guarantees enough are available.
pub(crate) fn drain_fifo(fifo: &mut Vec<f32>, samples: &mut [f32]) {
  let drain_len = samples.len().min(fifo.len());
  for (dst, src) in samples.iter_mut().zip(fifo.drain(..drain_len)) {
    *dst = src;
  }
  // Only reachable if a caller block exceeds the priming; output silence, never raw input.
  for s in samples.iter_mut().skip(drain_len) {
    *s = 0.0;
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use gamersequalizer_common::biquad::BiquadCascade;
  use gamersequalizer_common::profile::{BiquadBand, FilterType};

  #[test]
  fn test_ai_denoiser_smoke() {
    let mut isolator = AiVoiceIsolator::new();
    let mut buffer = vec![0.05f32; 960]; // 2 frames
    isolator.process_samples(&mut buffer);
    assert_eq!(buffer.len(), 960);
  }

  #[test]
  fn test_odd_block_size_never_leaks_raw_input() {
    // Regression: with blocks that are not a multiple of 480, the unprocessed
    // tail used to be passed through raw, mixing dry and denoised audio.
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_gate_threshold_db(-80.0);
    let mut first = vec![0.5f32; 300];
    isolator.process_samples(&mut first);
    assert!(first.iter().all(|&s| s == 0.0), "first block must be primed silence");

    for _ in 0..20 {
      let mut block = vec![0.5f32; 441];
      isolator.process_samples(&mut block);
      assert_eq!(block.len(), 441);
      assert!(block.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
    }
  }

  #[test]
  fn test_bypass_passes_through_and_reenable_resets() {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_strength(0.0);
    let mut block = vec![0.25f32; 480];
    isolator.process_samples(&mut block);
    assert!(block.iter().all(|&s| s == 0.25));

    isolator.set_strength(1.0);
    let mut block = vec![0.25f32; 480];
    isolator.process_samples(&mut block);
    assert!(block.iter().all(|&s| s == 0.0), "re-enable starts from primed silence");
  }

  #[test]
  fn test_deepfilter_loads_and_switches_both_ways() {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_gate_threshold_db(-80.0);
    isolator.set_model(DenoiseModel::DeepFilterNet);
    let mut block = vec![0.01f32; 480];
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::DeepFilterNet, "load error: {:?}", isolator.df_error);

    let mut block = vec![0.1f32; 441];
    isolator.process_samples(&mut block);
    assert!(block.iter().all(|s| s.is_finite()));

    isolator.set_model(DenoiseModel::RnNoise);
    let mut block = vec![0.1f32; 480];
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
  }

  #[test]
  fn test_overload_falls_back_until_reselected() {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(DenoiseModel::DeepFilterNet);
    let mut block = vec![0.01f32; 480];
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::DeepFilterNet);

    isolator.fall_back_overloaded();
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
    assert_eq!(isolator.limit(), DenoiseLimit::Overloaded);

    // Same model again (the per-block settings sync) must not undo the fallback...
    isolator.set_model(DenoiseModel::DeepFilterNet);
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
    // ...picking another model and back does.
    isolator.set_model(DenoiseModel::RnNoise);
    isolator.set_model(DenoiseModel::DeepFilterNet);
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::DeepFilterNet);
    assert_eq!(isolator.limit(), DenoiseLimit::None);
  }

  #[test]
  fn test_measured_latency_matches_constants() {
    for model in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
      let measured = measure_latency_samples(model);
      let expected = (AiVoiceIsolator::latency_ms(model) * 48.0) as usize;
      eprintln!("{model:?}: measured latency {measured} samples ({:.1} ms)", measured as f32 / 48.0);
      // Even at minimal strength the heavy models still output ~half wet signal; DPDFNet's
      // filtering shifts that half's phase by a few samples.
      let tolerance = if model == DenoiseModel::DpdfNet { 4 } else { 2 };
      assert!(
        measured.abs_diff(expected) <= tolerance,
        "{model:?}: measured {measured} samples, constant says {expected}"
      );
    }
  }

  #[test]
  fn test_dpdfnet_loads_and_switches_both_ways() {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_gate_threshold_db(-80.0);
    isolator.set_model(DenoiseModel::DpdfNet);
    let mut block = vec![0.01f32; 480];
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::DpdfNet, "load error: {:?}", isolator.dpdf_error);
    let mut block = vec![0.1f32; 441];
    isolator.process_samples(&mut block);
    assert!(block.iter().all(|s| s.is_finite()));
    isolator.set_model(DenoiseModel::DeepFilterNet);
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::DeepFilterNet);
    isolator.set_model(DenoiseModel::RnNoise);
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
  }

  #[test]
  fn test_dpdfnet_overload_falls_back_to_rnnoise() {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(DenoiseModel::DpdfNet);
    let mut block = vec![0.01f32; 480];
    isolator.process_samples(&mut block);
    isolator.fall_back_overloaded();
    isolator.process_samples(&mut block);
    assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
    assert_eq!(isolator.limit(), DenoiseLimit::Overloaded);
  }

  #[test]
  fn test_dpdfnet_wet_and_dry_paths_are_aligned() {
    // The strength blend mixes the input back in; if its delay differs from the model's, the
    // two comb-filter. The dry path is checked by the latency test (strength ~0); here the
    // fully wet output of near-clean speech must match the speech best at the same delay,
    // clearly better than one hop (10 ms) earlier or later.
    let (clean, mixed) = speech_over_keyboard(-25.0, 20.0);
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(DenoiseModel::DpdfNet);
    isolator.set_strength(1.0);
    isolator.set_gate_threshold_db(GATE_OFF_DB - 1.0);
    let mut out = mixed.clone();
    for block in out.chunks_mut(441) {
      isolator.process_samples(block);
    }
    let corr = |lag: usize| -> f32 {
      let num: f32 = clean.iter().zip(out.iter().skip(lag)).map(|(a, b)| a * b).sum();
      let den = (clean.iter().map(|a| a * a).sum::<f32>() * out.iter().skip(lag).map(|b| b * b).sum::<f32>()).sqrt();
      num / den.max(1e-12)
    };
    let expected = (AiVoiceIsolator::latency_ms(DenoiseModel::DpdfNet) * 48.0) as usize;
    let best_near = |center: usize| {
      (center - 24..=center + 24).map(|l| (l, corr(l))).fold((0, f32::MIN), |b, c| if c.1 > b.1 { c } else { b })
    };
    let (at, here) = best_near(expected);
    let (_, early) = best_near(expected - 480);
    let (_, late) = best_near(expected + 480);
    eprintln!("wet correlation: {here:.3} at lag {at} (expected {expected}), one hop early {early:.3}, late {late:.3}");
    assert!(at.abs_diff(expected) <= 4, "best lag {at}, expected {expected}");
    assert!(here > 0.8 && here > early + 0.2 && here > late + 0.2, "{here} vs {early} / {late}");
  }

  #[test]
  fn test_dpdfnet_keeps_soft_speech_over_keyboard() {
    let (clean, mixed) = speech_over_keyboard(-30.0, -10.0);
    let dpdf = run_quality(DenoiseModel::DpdfNet, GATE_OFF_DB - 1.0, &clean, &mixed);
    let dfn = run_quality(DenoiseModel::DeepFilterNet, GATE_OFF_DB - 1.0, &clean, &mixed);
    eprintln!("loud keys, no gate: DPDFNet {dpdf:?}, DFN3 {dfn:?}");
    assert!(dpdf.dropouts < 0.01 && dpdf.soft_dropouts < 0.10, "{dpdf:?}");
    assert!(dpdf.noise_suppression_db > 35.0, "{dpdf:?}");
    let (clean, mixed) = speech_over_keyboard(-45.0, 0.0);
    let dpdf = run_quality(DenoiseModel::DpdfNet, GATE_OFF_DB - 1.0, &clean, &mixed);
    let dfn = run_quality(DenoiseModel::DeepFilterNet, GATE_OFF_DB - 1.0, &clean, &mixed);
    eprintln!("quiet voice, no gate: DPDFNet {dpdf:?}, DFN3 {dfn:?}");
    assert!(dpdf.soft_dropouts <= dfn.soft_dropouts, "{dpdf:?} vs DFN3 {dfn:?}");
  }

  #[test]
  fn test_deepfilter_keeps_soft_speech_over_keyboard() {
    // Regression: upstream min_db_thresh (-10 dB) zeroed 26 % of soft syllables here.
    let (clean, mixed) = speech_over_keyboard(-30.0, -10.0);
    let q = run_quality(DenoiseModel::DeepFilterNet, GATE_OFF_DB - 1.0, &clean, &mixed);
    eprintln!("DFN, loud keys: {q:?}");
    assert!(q.dropouts < 0.01 && q.soft_dropouts < 0.10, "{q:?}");
    assert!(q.noise_suppression_db > 35.0, "{q:?}");
  }

  #[test]
  fn test_gate_keeps_loud_and_quiet_speech() {
    // Regression: the old level gate at its default -45 dB cut 13 % of a quiet voice
    // (-45 dBFS) and half of its soft syllables. The automatic gate has no level setting:
    // a loud (daytime) and a quiet (evening) voice must both pass like with no gate.
    for level in [-20.0f32, -45.0] {
      let (clean, mixed) = speech_over_keyboard(level, 0.0);
      for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
        let with_gate = run_quality(model, GATE_AUTO, &clean, &mixed);
        let no_gate = run_quality(model, GATE_OFF_DB - 1.0, &clean, &mixed);
        eprintln!("{model:?} voice {level} dBFS: gate {with_gate:?}, no gate {no_gate:?}");
        assert!(with_gate.dropouts <= no_gate.dropouts + 0.01, "{model:?} {level}: {with_gate:?} vs {no_gate:?}");
        assert!(
          with_gate.soft_dropouts <= no_gate.soft_dropouts + 0.03,
          "{model:?} {level}: {with_gate:?} vs {no_gate:?}"
        );
      }
    }
  }

  #[test]
  fn test_gate_keeps_soft_syllables_under_loud_keys() {
    // Regression: with one 0.5 threshold the gate closed on soft syllables between louder
    // ones (+3.5 % cut for DFN3, +5.3 % for RNNoise over no gate). Hysteresis keeps them.
    let (clean, mixed) = speech_over_keyboard(-30.0, -10.0);
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
      let with_gate = run_quality(model, GATE_AUTO, &clean, &mixed);
      let no_gate = run_quality(model, GATE_OFF_DB - 1.0, &clean, &mixed);
      eprintln!("{model:?} loud keys: gate {with_gate:?}, no gate {no_gate:?}");
      assert!(with_gate.soft_dropouts <= no_gate.soft_dropouts + 0.015, "{model:?}: {with_gate:?} vs {no_gate:?}");
    }
  }

  #[test]
  fn test_gate_hold_covers_model_latency() {
    // Regression: the hold ran from when the detector heard the raw input, a model latency
    // before that audio leaves the model, so DPDFNet (60 ms) lost 1.9 -> 3.1 % of a quiet
    // voice's soft syllables to the gate. Now 2.5 % (DFN3 5.3 -> 5.6 %); the rest are onsets
    // the detector recognises late (a 400 ms hold changes nothing).
    let (clean, mixed) = speech_over_keyboard(-45.0, 0.0);
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
      let with_gate = run_quality(model, GATE_AUTO, &clean, &mixed);
      let no_gate = run_quality(model, GATE_OFF_DB - 1.0, &clean, &mixed);
      eprintln!("{model:?} quiet voice: gate {with_gate:?}, no gate {no_gate:?}");
      assert!(with_gate.soft_dropouts <= no_gate.soft_dropouts + 0.008, "{model:?}: {with_gate:?} vs {no_gate:?}");
    }
  }

  #[test]
  fn test_gate_shuts_on_typing_without_speech() {
    // Max open share per model (DFN3, DPDFNet, RNNoise). The gate favours the voice: the output
    // detector also hears keys a model leaves in, so RNNoise (keys still audible in its output)
    // opens on a third of loud typing (measured 31 %, 11 % at -40 dBFS); DFN3 5 %, DPDFNet 2 %.
    // Closed or not, keys stay `GATE_FLOOR_DB` or more under the model's output.
    for (level, max_open) in [(-20.0f32, [0.08f32, 0.03, 0.40]), (-40.0, [0.0, 0.0, 0.15])] {
      let keys = keyboard_only(level);
      for (model, max_open) in
        [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise].into_iter().zip(max_open)
      {
        let open = open_share(&run(model, GATE_AUTO, &keys), &run(model, GATE_OFF_DB - 1.0, &keys));
        eprintln!("{model:?} keys only {level} dBFS: gate open {:.1} % of the time", open * 100.0);
        assert!(open <= max_open, "{model:?} {level}: open {open}");
      }
    }
  }

  #[test]
  fn test_closed_gate_attenuates_by_floor_not_mute() {
    let mut gate = NoiseGate::new(true);
    let mut block = vec![1.0f32; 48_000];
    gate.process(&mut block, 0);
    let last = block.last().copied().unwrap_or_default();
    assert!((db(last) - GATE_FLOOR_DB).abs() < 0.1, "{} dB", db(last));
  }

  /// Share of 10 ms frames (after a 1 s warm-up) in which the gate is open: the gated output
  /// is within 6 dB of the same model's ungated output (closed = `GATE_FLOOR_DB` under it).
  fn open_share(gated: &[f32], ungated: &[f32]) -> f32 {
    let frames: Vec<bool> = gated
      .as_chunks::<480>()
      .0
      .iter()
      .zip(ungated.as_chunks::<480>().0)
      .skip(100)
      .map(|(g, u)| rms(u) > 1e-6 && db(rms(g)) > db(rms(u)) - 6.0)
      .collect();
    frames.iter().filter(|&&o| o).count() as f32 / frames.len().max(1) as f32
  }

  /// What the UI writes for "Auto".
  const GATE_AUTO: f32 = 0.0;

  /// Default strength, 10 ms blocks like the audio engine.
  fn run(model: DenoiseModel, gate_db: f32, input: &[f32]) -> Vec<f32> {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(model);
    isolator.set_gate_threshold_db(gate_db);
    let mut out = input.to_vec();
    for block in out.chunks_mut(480) {
      isolator.process_samples(block);
    }
    assert_eq!(isolator.active_model(), model);
    out
  }

  fn run_quality(model: DenoiseModel, gate_db: f32, clean: &[f32], mixed: &[f32]) -> Quality {
    let latency = (AiVoiceIsolator::latency_ms(model) * 48.0) as usize;
    measure(clean, mixed, &run(model, gate_db, mixed), latency)
  }

  const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test_data/denoise");
  const KEYBOARD_CLIPS: [&str; 6] = [
    "1-137-A-32.wav",
    "1-53501-A-32.wav",
    "1-118206-A-31.wav",
    "1-61212-A-32.wav",
    "1-18655-A-31.wav",
    "1-79711-A-32.wav",
  ];

  #[derive(Debug)]
  struct Quality {
    /// Share of loud speech frames the output lost (more than 15 dB below the clean speech).
    dropouts: f32,
    /// Same as `dropouts` for soft speech (syllable onsets/tails, up to 15 dB below average).
    soft_dropouts: f32,
    /// How much quieter the output is than the input in speech pauses (keyboard only), dB.
    noise_suppression_db: f32,
  }

  /// Clean speech at `speech_rms_db` dBFS and the same speech with mechanical keyboard and
  /// mouse clicks mixed in at `snr_db`. Both at 48 kHz.
  fn speech_over_keyboard(speech_rms_db: f32, snr_db: f32) -> (Vec<f32>, Vec<f32>) {
    mix("speech_clean.wav", &KEYBOARD_CLIPS, None, speech_rms_db, snr_db)
  }

  /// `speech_over_keyboard` for any fixtures (paths relative to `FIXTURES`). `low_cut_hz`
  /// high-passes the speech first (24 dB/oct, like the mic EQ's low cut): breath pops and
  /// desk bumps in a raw recording are loud but not speech, and would count as lost speech.
  fn mix(
    speech: &str,
    noise_clips: &[&str],
    low_cut_hz: Option<f32>,
    speech_rms_db: f32,
    snr_db: f32,
  ) -> (Vec<f32>, Vec<f32>) {
    let mut clean = read_wav_48k(speech);
    if let Some(frequency) = low_cut_hz {
      let mut eq = BiquadCascade::default();
      let band =
        BiquadBand { filter_type: FilterType::HighPass, frequency, q: 0.707, slope_db: 24, ..BiquadBand::default() };
      eq.set_bands(&[band], 48_000.0);
      eq.process(&mut clean);
    }
    let mut noise: Vec<f32> = noise_clips.iter().flat_map(|f| read_wav_48k(f)).collect();
    while noise.len() < clean.len() {
      noise.extend_from_within(..);
    }
    noise.truncate(clean.len());
    let clean = scale_to_db(&clean, speech_rms_db);
    let noise = scale_to_db(&noise, speech_rms_db - snr_db);
    let mixed = clean.iter().zip(&noise).map(|(s, n)| s + n).collect();
    (clean, mixed)
  }

  /// Keyboard and mouse clicks only (no speech) at `rms_db` dBFS, 48 kHz.
  fn keyboard_only(rms_db: f32) -> Vec<f32> {
    noise_only(&KEYBOARD_CLIPS, rms_db)
  }

  fn noise_only(noise_clips: &[&str], rms_db: f32) -> Vec<f32> {
    let noise: Vec<f32> = noise_clips.iter().flat_map(|f| read_wav_48k(f)).collect();
    scale_to_db(&noise, rms_db)
  }

  /// Compares the output (delayed by the model's latency) with the clean speech, per 10 ms frame.
  fn measure(clean: &[f32], mixed: &[f32], out: &[f32], latency: usize) -> Quality {
    const FRAME: usize = 480;
    const WARMUP_FRAMES: usize = 50;
    let delayed = &out[latency..];
    let clean_db = db(rms(clean));
    let (mut loud, mut lost, mut soft, mut soft_lost) = (0usize, 0usize, 0usize, 0usize);
    let (mut pause_in, mut pause_out) = (0.0f32, 0.0f32);
    for ((c, m), o) in clean
      .as_chunks::<FRAME>()
      .0
      .iter()
      .zip(mixed.as_chunks::<FRAME>().0)
      .zip(delayed.as_chunks::<FRAME>().0)
      .skip(WARMUP_FRAMES)
    {
      let c_db = db(rms(c));
      let is_lost = db(rms(o)) - c_db < -15.0;
      if c_db >= clean_db {
        loud += 1;
        lost += is_lost as usize;
      } else if c_db >= clean_db - 15.0 {
        soft += 1;
        soft_lost += is_lost as usize;
      } else if c_db < clean_db - 40.0 {
        pause_in += m.iter().map(|x| x * x).sum::<f32>();
        pause_out += o.iter().map(|x| x * x).sum::<f32>();
      }
    }
    Quality {
      dropouts: lost as f32 / loud.max(1) as f32,
      soft_dropouts: soft_lost as f32 / soft.max(1) as f32,
      noise_suppression_db: 10.0 * (pause_in.max(1e-12) / pause_out.max(1e-12)).log10(),
    }
  }

  /// Mono WAV fixture as f32, linearly resampled to 48 kHz (good enough for test noise).
  fn read_wav_48k(name: &str) -> Vec<f32> {
    let mut reader = hound::WavReader::open(format!("{FIXTURES}/{name}")).unwrap();
    let spec = reader.spec();
    assert_eq!(spec.channels, 1, "{name}");
    let samples: Vec<f32> = match spec.sample_format {
      hound::SampleFormat::Int => {
        let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
        reader.samples::<i32>().map(|s| s.unwrap() as f32 / scale).collect()
      }
      hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
    };
    if spec.sample_rate == 48_000 {
      return samples;
    }
    let step = spec.sample_rate as f64 / 48_000.0;
    let len = ((samples.len() - 1) as f64 / step) as usize;
    (0..len)
      .map(|i| {
        let pos = i as f64 * step;
        let (j, frac) = (pos as usize, (pos.fract()) as f32);
        samples[j] * (1.0 - frac) + samples[j + 1] * frac
      })
      .collect()
  }

  fn scale_to_db(x: &[f32], target_db: f32) -> Vec<f32> {
    let gain = 10f32.powf(target_db / 20.0) / rms(x).max(1e-9);
    x.iter().map(|s| s * gain).collect()
  }

  fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
  }

  fn db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
  }

  /// Runs noise through the model at minimal strength (mostly dry, so the signal survives) in
  /// odd-sized blocks and finds the input->output lag by cross-correlation.
  fn measure_latency_samples(model: DenoiseModel) -> usize {
    const LEN: usize = 48_000;
    const MAX_LAG: usize = 3_000;
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(model);
    isolator.set_strength(0.01);
    isolator.set_gate_threshold_db(-80.0);
    let mut x = 12_345u32;
    let input: Vec<f32> = (0..LEN)
      .map(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.2
      })
      .collect();
    let mut output = input.clone();
    for block in output.chunks_mut(441) {
      isolator.process_samples(block);
    }
    assert_eq!(isolator.active_model(), model);
    const SKIP: usize = 6_000; // model warm-up; must be >= MAX_LAG
    let window = LEN - SKIP;
    (0..MAX_LAG)
      .map(|lag| {
        let dry = input.iter().skip(SKIP - lag);
        let c: f32 = dry.zip(output.iter().skip(SKIP)).take(window).map(|(a, b)| a * b).sum();
        (lag, c)
      })
      .fold((0, f32::MIN), |best, cur| if cur.1 > best.1 { cur } else { best })
      .0
  }

  /// Report on the developer's own recordings in the git-ignored `test_data/private/`
  /// (`speech.wav`, `keys.wav`: mono WAV). Prints gate vs no gate per model, no thresholds:
  /// a private voice can't back a shared regression test. Run:
  /// `cargo test -p gamersequalizer-mic-vst --release private_report -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn private_report() {
    const SPEECH: &str = "../private/speech.wav";
    const KEYS: [&str; 1] = ["../private/keys.wav"];
    if [SPEECH, KEYS[0]].iter().any(|f| !std::path::Path::new(FIXTURES).join(f).exists()) {
      eprintln!("skipped: put speech.wav and keys.wav into test_data/private/");
      return;
    }
    eprintln!("soft% / loud% lost [pause suppression dB], gate auto | no gate");
    for (level, snr) in [(-20.0f32, 0.0f32), (-45.0, 0.0), (-30.0, -10.0)] {
      let (clean, mixed) = mix(SPEECH, &KEYS, Some(100.0), level, snr);
      for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
        let row = |q: Quality| {
          format!("{:4.1} / {:3.1} [{:4.1}]", q.soft_dropouts * 100.0, q.dropouts * 100.0, q.noise_suppression_db)
        };
        let gate = row(run_quality(model, GATE_AUTO, &clean, &mixed));
        let no_gate = row(run_quality(model, GATE_OFF_DB - 1.0, &clean, &mixed));
        eprintln!("voice {level:>4} dBFS, SNR {snr:>4} dB, {model:<13?} {gate} | {no_gate}");
      }
    }
    for level in [-20.0f32, -40.0] {
      let keys = noise_only(&KEYS, level);
      for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
        let open = open_share(&run(model, GATE_AUTO, &keys), &run(model, GATE_OFF_DB - 1.0, &keys));
        eprintln!("keys only {level} dBFS, {model:<13?} gate open {:.1} %", open * 100.0);
      }
    }
  }

  /// Tuning report for the detector's hysteresis (keep threshold x active window). Run:
  /// `cargo test -p gamersequalizer-mic-vst --release gate_sweep -- --ignored --nocapture`
  #[test]
  #[ignore = "tuning report, slow"]
  fn gate_sweep() {
    let run_with = |model: DenoiseModel, keep: f32, active: u32, input: &[f32]| -> Vec<f32> {
      let mut isolator = AiVoiceIsolator::new();
      isolator.set_model(model);
      isolator.set_gate_threshold_db(GATE_AUTO);
      isolator.vad.keep_prob = keep;
      isolator.vad.active_frames = active;
      let mut out = input.to_vec();
      for block in out.chunks_mut(480) {
        isolator.process_samples(block);
      }
      out
    };
    let speech = [(-30.0f32, -10.0f32), (-20.0, 0.0), (-45.0, 0.0)].map(|(l, s)| speech_over_keyboard(l, s));
    // Speech, then typing only: how long after the last word the gate lets keys through.
    let (clean, _) = speech_over_keyboard(-30.0, 0.0);
    let last_loud = clean.iter().rposition(|s| s.abs() > 0.01).unwrap_or(clean.len());
    let typing = keyboard_only(-20.0);
    let mut talk_then_type: Vec<f32> = clean[..last_loud].to_vec();
    let speech_end = talk_then_type.len();
    talk_then_type.extend_from_slice(&typing);
    // How often non-speech scores above a keep threshold (it would hold the gate open).
    let mut x = 1u32;
    let mut lp = 0.0f32;
    let fan: Vec<f32> = (0..48_000 * 10)
      .map(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        lp += 0.05 * (((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) - lp);
        lp
      })
      .collect();
    for (name, sig) in [
      ("keys -20", keyboard_only(-20.0)),
      ("keys -40", keyboard_only(-40.0)),
      ("fan -50", scale_to_db(&fan, -50.0)),
      ("fan -30", scale_to_db(&fan, -30.0)),
    ] {
      let mut st = DenoiseState::new();
      let mut scratch = [0.0f32; FRAME_SIZE];
      let probs: Vec<f32> = sig
        .as_chunks::<FRAME_SIZE>()
        .0
        .iter()
        .skip(50)
        .map(|f| st.process_frame(&mut scratch, &f.map(|s| s * I16_SCALE)))
        .collect();
      let share = |t: f32| probs.iter().filter(|&&p| p > t).count() as f32 / probs.len() as f32 * 100.0;
      eprintln!(
        "{name}: frames over 0.05 {:.1} %, 0.1 {:.1} %, 0.2 {:.1} %, 0.5 {:.1} %",
        share(0.05),
        share(0.1),
        share(0.2),
        share(0.5)
      );
    }
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::RnNoise] {
      let latency = (AiVoiceIsolator::latency_ms(model) * 48.0) as usize;
      eprintln!(
        "
{model:?}  soft%/loud% lost [pause suppression dB]: keys-10dB | -20dBFS | -45dBFS ; open% while typing after speech"
      );
      for (active, keep) in
        [(20u32, 0.5f32), (20, 0.1), (20, 0.05), (10, 0.1), (10, 0.05), (5, 0.1), (5, 0.05), (5, 0.2)]
      {
        let q: Vec<String> = speech
          .iter()
          .map(|(c, m)| {
            let q = measure(c, m, &run_with(model, keep, active, m), latency);
            format!("{:4.1}/{:3.1} [{:4.1}]", q.soft_dropouts * 100.0, q.dropouts * 100.0, q.noise_suppression_db)
          })
          .collect();
        let out = run_with(model, keep, active, &talk_then_type);
        let ungated = run(model, GATE_OFF_DB - 1.0, &talk_then_type);
        let tail = open_share(&out[speech_end - 48_000..], &ungated[speech_end - 48_000..]); // skips its first 1 s
        eprintln!("  active {active:2} keep {keep:4}: {} ; {:4.1}", q.join(" | "), tail * 100.0);
      }
      for (c, m) in &speech {
        let q = measure(c, m, &run(model, GATE_OFF_DB - 1.0, m), latency);
        eprint!(
          "  no gate: {:4.1}/{:3.1} [{:4.1}]",
          q.soft_dropouts * 100.0,
          q.dropouts * 100.0,
          q.noise_suppression_db
        );
      }
      eprintln!();
    }
  }
}
