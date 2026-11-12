use crate::aec3::{EchoCanceller3, PRE_DELAY_SAMPLES};
use crate::ai_denoiser::{AiVoiceIsolator, DenoiseDebug};
use crate::clip_detector::ClipDetector;
use crate::delay_estimator::{DelayEstimator, LockChange, WindowOutcome, FULL_SCALE_PEAK_RATIO, MIN_PEAK_RATIO};
use crate::latency_meter::LatencyMeter;
use crate::shared_memory::SharedMemoryClient;
use crate::voice_leveler::VoiceLeveler;
use gamersequalizer_common::biquad::BiquadCascade;
use gamersequalizer_common::limiter::PeakLimiter;
use gamersequalizer_common::profile::{BiquadBand, FilterType};
use gamersequalizer_common::shm::{
  AecState, DenoiseModel, ShmHeader, StereoFrame, AEC_IMPULSE_POINTS, MIC_CHAIN_ASLEEP, MIC_CHAIN_BYPASSED,
  MIC_CHAIN_LOOKAHEAD, MIC_CHAIN_OTHERS, MIC_CHAIN_STEREO_REF, MIC_CHAIN_TAKEOVER, MIC_LEVELER_DEFAULT_TARGET_DB_X10,
  NOISE_GATE_AUTO_LOOKAHEAD,
};
use std::sync::atomic::Ordering;

#[cfg(feature = "debug-capture")]
use crate::debug_capture::{CaptureWriter, DebugCaptureMap};
#[cfg(feature = "debug-capture")]
use gamersequalizer_common::debug_capture::*;

/// Publish telemetry every 10 blocks (~100 ms): enough for a smooth UI, negligible cost.
const TELEMETRY_EVERY_BLOCKS: u32 = 10;
/// Reference older than 200 ms means nothing is playing (loopback delivers no packets).
const REF_STALE_HNS: u64 = 2_000_000;
/// Below this RMS the reference counts as silent for the UI state.
const REF_SILENT_RMS: f32 = 1e-4;
/// A telemetry window is "overloaded" with this many late blocks (processing > block length)...
const OVERLOAD_LATE_BLOCKS: u32 = 2;
/// ...or this average load (time budget share; the audio engine needs the rest).
const OVERLOAD_AVG_LOAD: f32 = 0.9;
/// Consecutive overloaded windows (~100 ms each) before a heavy model gives way to RNNoise.
const OVERLOAD_WINDOWS_TO_FALL_BACK: u32 = 3;
/// Fixed low cut before the denoiser: 2nd-order Butterworth, below any voice's fundamental.
const PRE_HIGH_PASS_HZ: f32 = 70.0;
/// Window of the published chain extremes (probability max, gate / limiter min): 250 ms at 48 kHz,
/// one daemon log tick.
const CHAIN_WINDOW_SAMPLES: usize = 12_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginMode {
  Auto,
  SpeakerTap,
  MicProcessor,
}

pub struct GamersEqualizerPlugin {
  pub mode: PluginMode,
  shm_client: Option<SharedMemoryClient>,
  delay_estimator: DelayEstimator,
  aec: EchoCanceller3,
  /// Measures the delay the whole chain adds (raw input vs final output), for the UI/log.
  latency_meter: LatencyMeter,
  /// Sum of the stage latencies at the last block: a change (model, lookahead, AEC on/off)
  /// throws the old measurement away.
  latency_stages: usize,
  /// Fixed low cut in front of the detectors and the denoiser (desk thumps, air conditioning);
  /// the user EQ's low cut only comes after them.
  pre_high_pass: BiquadCascade,
  ai_denoiser: AiVoiceIsolator,
  leveler: VoiceLeveler,
  /// Last "Learn again" sequence seen (None before the first block).
  leveler_reset_seen: Option<u32>,
  /// `denoise_retry_seq` last block (None = not read yet: the first value is no request).
  denoise_retry_seen: Option<u32>,
  /// After "Learn again" in this stream, the saved level is stale until the daemon clears it.
  leveler_seed_blocked: bool,
  /// Settings before the leveler (they change the speech level it sees), last block.
  leveler_inputs: Option<LevelerInputs>,
  eq: BiquadCascade,
  eq_version: Option<u32>,
  /// Last stage: volume, then this (smooth gain on peaks instead of clipping them).
  limiter: PeakLimiter,
  pub sample_rate: f32,
  /// Speaker reference of the block as the AEC gets it: mono mix, and left / right.
  ref_buffer: Vec<f32>,
  ref_stereo: [Vec<f32>; 2],
  mic_mono_temp: Vec<f32>,
  tap_frames: Vec<StereoFrame>,
  aec_state: AecState,
  /// Last `aec_cached_delay_samples` seen in shared memory (warm start when it changes).
  cached_delay_seen: u32,
  /// Last `aec_cached_echo_db_x10` seen in shared memory (0 = none).
  cached_echo_seen: i32,
  /// Last `aec_reference_seq` seen: when it changes, the speaker output was switched.
  reference_seq_seen: u32,
  /// The chain did not run on the last block(s) (master bypass, or the APO passed through):
  /// the AEC and denoiser FIFOs hold audio from before, which would play first on resume.
  paused: bool,
  /// `aec.process_block` did not run on the last block (no reference): same, for the AEC alone.
  aec_idle: bool,
  block_counter: u32,
  /// False: another mic chain in this process publishes the telemetry, this one only
  /// processes (two would overwrite each other's values in the shared memory).
  publishes: bool,
  /// More mic chains than this one run in this process (`MIC_CHAIN_OTHERS`).
  others: bool,
  ref_rms: f32,
  mic_rms: f32,
  out_rms: f32,
  load: DspLoad,
  /// Clipped raw input samples (full scale or flat tops), all time (wraps).
  input_clips: u32,
  clip_detector: ClipDetector,
  /// Shift of the reference fed to the AEC in the last block (None: the AEC didn't run).
  aec_ref_offset: Option<u32>,
  /// Chain state for the daemon's debug log.
  chain: ChainWindow,
  #[cfg(feature = "debug-capture")]
  capture: Option<CaptureWriter>,
  /// Tests without shared memory: (model, mic volume permille) instead of the defaults.
  #[cfg(test)]
  test_overrides: Option<(DenoiseModel, u32)>,
}

/// Everything before the leveler that changes the speech level it learns: when any of it changes,
/// the learned level is stale. Raw shared-memory values, so a slider drag is "changed" every block.
/// The Windows mic volume is not part of it: the leveler is told its gain and compensates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LevelerInputs {
  aec_enabled: bool,
  ai_strength_permille: u32,
  gate_db: i32,
  /// The model that runs (a fallback to RNNoise counts too).
  active_model: u32,
  eq_version: Option<u32>,
}

/// CPU time of the mic chain relative to real time, per telemetry window.
#[derive(Default)]
struct DspLoad {
  peak: f32,
  sum: f32,
  blocks: u32,
  late_in_window: u32,
  /// Cumulative late blocks (published; the UI/log compares successive values).
  late_total: u32,
  overloaded_windows: u32,
}

/// Chain state for the daemon's debug log: cumulative block counters, extremes per window.
#[derive(Default)]
struct ChainWindow {
  voice_blocks: u32,
  blocks: u32,
  rescue_blocks: u32,
  samples: usize,
  /// Extremes of the running window; `last` holds the finished one's (the published values).
  now: ChainExtremes,
  last: ChainExtremes,
}

#[derive(Clone, Copy)]
struct ChainExtremes {
  in_prob: f32,
  out_prob: f32,
  soft_prob: f32,
  /// Linear gains, 1.0 = no reduction.
  gate_gain: f32,
  limiter_gain: f32,
}

impl GamersEqualizerPlugin {
  pub fn new(sample_rate: f32) -> Self {
    Self::with_shm(sample_rate, SharedMemoryClient::open_or_create().ok())
  }

  /// Uses the given mapping for settings, reference audio and telemetry (the APO passes
  /// its Global mapping here).
  pub fn with_shm(sample_rate: f32, shm_client: Option<SharedMemoryClient>) -> Self {
    Self {
      mode: PluginMode::Auto,
      shm_client,
      delay_estimator: DelayEstimator::new(),
      aec: EchoCanceller3::new(),
      latency_meter: LatencyMeter::new(),
      latency_stages: 0,
      pre_high_pass: pre_high_pass(),
      ai_denoiser: AiVoiceIsolator::new(),
      leveler: VoiceLeveler::new(),
      leveler_reset_seen: None,
      denoise_retry_seen: None,
      leveler_seed_blocked: false,
      leveler_inputs: None,
      eq: BiquadCascade::default(),
      eq_version: None,
      limiter: PeakLimiter::new(48_000.0),
      sample_rate,
      ref_buffer: vec![0.0; 2048],
      ref_stereo: [vec![0.0; 2048], vec![0.0; 2048]],
      mic_mono_temp: vec![0.0; 2048],
      tap_frames: Vec::with_capacity(2048),
      aec_state: AecState::Disabled,
      cached_delay_seen: 0,
      cached_echo_seen: 0,
      reference_seq_seen: 0,
      paused: false,
      aec_idle: false,
      block_counter: 0,
      publishes: true,
      others: false,
      ref_rms: 0.0,
      mic_rms: 0.0,
      out_rms: 0.0,
      load: DspLoad::default(),
      input_clips: 0,
      clip_detector: ClipDetector::default(),
      aec_ref_offset: None,
      chain: ChainWindow::default(),
      #[cfg(feature = "debug-capture")]
      capture: None,
      #[cfg(test)]
      test_overrides: None,
    }
  }

  /// Debug builds: record every stage into `map` when the capture tool asks for it.
  #[cfg(feature = "debug-capture")]
  pub fn attach_debug_capture(&mut self, map: DebugCaptureMap, device_rate: u32) {
    self.capture = Some(CaptureWriter::new(map, device_rate));
  }

  /// Warm-start the echo canceller with a delay measured in an earlier session.
  pub fn set_cached_aec_delay(&mut self, delay_samples: usize) {
    self.delay_estimator.set_cached_delay(delay_samples);
  }

  /// Confirmed echo delay (48 kHz samples), for persisting across restarts.
  pub fn confirmed_aec_delay(&self) -> Option<usize> {
    if self.delay_estimator.is_from_cache() {
      None // nothing new learned
    } else {
      self.delay_estimator.current_delay()
    }
  }

  /// Apply the noise model chosen in the UI now (a heavy model loads here, 0.1-0.2 s).
  /// Call before audio starts so the first selection doesn't cause a gap.
  pub fn preload_selected_model(&mut self) {
    if let Some(model) = self.shm_header().map(|h| DenoiseModel::from_u32(h.denoise_model.load(Ordering::Relaxed))) {
      self.ai_denoiser.set_model(model);
    }
  }

  /// Process audio buffers without timestamps (VST host). AEC needs timestamps, so it
  /// reports `NoReference` in this path.
  pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
    self.process_timed(inputs, outputs, None);
  }

  /// `mic_qpc_hns`: QPC time (100 ns units) of the first input frame, from WASAPI.
  pub fn process_timed(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], mic_qpc_hns: Option<u64>) {
    let Some(first) = inputs.first() else {
      return;
    };
    let frames = first.len();
    if frames == 0 || outputs.is_empty() {
      return;
    }

    let effective_mode = match self.mode {
      PluginMode::Auto if inputs.len() >= 2 => PluginMode::SpeakerTap,
      PluginMode::Auto => PluginMode::MicProcessor,
      m => m,
    };

    if effective_mode == PluginMode::SpeakerTap {
      self.speaker_tap(inputs, outputs);
    } else {
      self.mic_process(first, outputs, mic_qpc_hns);
    }
  }

  fn speaker_tap(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
    // Pass-through audio to output untouched
    for (ch, out) in outputs.iter_mut().enumerate() {
      if let Some(src) = inputs.get(ch.min(inputs.len().saturating_sub(1))) {
        let n = out.len().min(src.len());
        if let (Some(o), Some(s)) = (out.get_mut(..n), src.get(..n)) {
          o.copy_from_slice(s);
        }
      }
    }

    // Tap stereo mix into shared memory ring buffer
    if let Some(shm) = self.shm_client.as_mut() {
      let left = inputs.first().copied().unwrap_or(&[]);
      let right = inputs.get(1).copied().unwrap_or(left);
      self.tap_frames.clear();
      self.tap_frames.extend(left.iter().zip(right).map(|(&l, &r)| StereoFrame { left: l, right: r }));
      shm.ring_buffer().push_frames(&self.tap_frames);
    }
  }

  /// With several mic chains in one process only one may publish telemetry: `publishes` says
  /// whether it is this one, `others` whether more than this one run.
  pub fn set_publishing(&mut self, publishes: bool, others: bool) {
    self.publishes = publishes;
    self.others = others;
  }

  /// The host is skipping the chain for a while (passes the mic through): the next processed
  /// block starts with empty buffers instead of ~10-30 ms of audio from before the pause.
  pub fn pause(&mut self) {
    self.paused = true;
  }

  fn mic_process(&mut self, mic_in: &[f32], outputs: &mut [&mut [f32]], mic_qpc_hns: Option<u64>) {
    // Master switch "everything off": the raw microphone, no AEC/denoise/EQ/boost.
    if self.shm_header().is_some_and(|h| h.master_bypass.load(Ordering::Relaxed)) {
      self.paused = true;
      for out in outputs.iter_mut() {
        let n = out.len().min(mic_in.len());
        if let (Some(o), Some(s)) = (out.get_mut(..n), mic_in.get(..n)) {
          o.copy_from_slice(s);
        }
      }
      return;
    }
    // Real-time thread: only flushes that allocate nothing (a rebuilt denoiser or filter would).
    if std::mem::take(&mut self.paused) {
      self.aec.flush();
      self.ai_denoiser.flush();
      self.latency_meter.reset();
      self.pre_high_pass.reset_state();
      self.limiter = PeakLimiter::new(48_000.0);
    }
    let started = std::time::Instant::now();
    let frames = mic_in.len();
    if self.mic_mono_temp.len() < frames {
      self.mic_mono_temp.resize(frames, 0.0);
    }
    for buffer in std::iter::once(&mut self.ref_buffer).chain(&mut self.ref_stereo) {
      if buffer.len() < frames {
        buffer.resize(frames, 0.0);
      }
    }
    // Trust boundary: a driver glitch can deliver NaN/inf, and one non-finite sample would
    // stay in the AEC / leveler state for the rest of the stream. +-4 (12 dBFS) is far beyond
    // any real converter and keeps the float math finite.
    if let Some(dst) = self.mic_mono_temp.get_mut(..frames) {
      for (d, &s) in dst.iter_mut().zip(mic_in) {
        *d = if s.is_finite() { s.clamp(-4.0, 4.0) } else { 0.0 };
      }
      self.latency_meter.push_input(dst);
    }
    #[cfg(feature = "debug-capture")]
    let capturing = self.capture_input(mic_in, mic_qpc_hns);

    // Fetch control parameters from shared memory
    let mut aec_enabled = true;
    let mut ai_strength = 0.75f32;
    let mut gate_db = NOISE_GATE_AUTO_LOOKAHEAD as f32; // the default until settings are read
    let mut mic_permille = 1000u32;
    let mut model = DenoiseModel::RnNoise;
    let mut denoise_retry_seq = 0;
    let mut eq_version = None;
    let mut cached_delay = 0u32;
    let mut cached_echo_x10 = 0;
    let mut reference_seq = self.reference_seq_seen;
    let mut leveler_on = true;
    let mut input_gain_x10 = 0;
    let (mut leveler_target_x10, mut leveler_reset_seq, mut leveler_saved_x10) =
      (MIC_LEVELER_DEFAULT_TARGET_DB_X10, 0, 0);
    if let Some(header) = self.shm_header() {
      leveler_on = header.mic_leveler_enabled.load(Ordering::Relaxed);
      input_gain_x10 = header.mic_input_gain_db_x10.load(Ordering::Relaxed);
      leveler_target_x10 = header.mic_leveler_target_db_x10.load(Ordering::Relaxed);
      leveler_reset_seq = header.mic_leveler_reset_seq.load(Ordering::Relaxed);
      leveler_saved_x10 = header.mic_leveler_saved_level_db_x10.load(Ordering::Relaxed);
      // Acquire before the cached delay: the daemon stores the new output's delay first.
      reference_seq = header.aec_reference_seq.load(Ordering::Acquire);
      cached_delay = header.aec_cached_delay_samples.load(Ordering::Relaxed);
      cached_echo_x10 = header.aec_cached_echo_db_x10.load(Ordering::Relaxed);
      aec_enabled = header.aec_enabled.load(Ordering::Relaxed);
      ai_strength = header.ai_voice_strength_permille.load(Ordering::Relaxed) as f32 / 1000.0;
      gate_db = header.noise_gate_threshold_db.load(Ordering::Relaxed) as f32;
      let p = header.mic_volume_permille.load(Ordering::Relaxed);
      if p > 0 {
        mic_permille = p;
      }
      model = DenoiseModel::from_u32(header.denoise_model.load(Ordering::Relaxed));
      denoise_retry_seq = header.denoise_retry_seq.load(Ordering::Relaxed);
      eq_version = Some(header.mic_eq_version.load(Ordering::Acquire));
    }
    #[cfg(test)]
    if let Some((m, p)) = self.test_overrides {
      model = m;
      mic_permille = p;
    }

    // Speaker output switched: the locked delay and the learned echo path are the old
    // device's. Start over, warm-started below with the delay remembered for the new one.
    if reference_seq != self.reference_seq_seen {
      self.reference_seq_seen = reference_seq;
      log::info!("[AEC] speaker output changed: echo delay and filter start over");
      self.delay_estimator.forget_lock();
      // Before the reset, which rebuilds AEC3 for what is known of the echo.
      self.aec.forget_echo_strength();
      self.aec.reset();
      self.cached_delay_seen = 0;
      self.cached_echo_seen = 0;
    }

    // Delay remembered from an earlier session (daemon cache): only as a warm start, a delay
    // measured in this session wins.
    if cached_delay != self.cached_delay_seen {
      self.cached_delay_seen = cached_delay;
      if cached_delay > 0 && self.delay_estimator.current_delay().is_none() {
        log::info!("[AEC] warm start with cached echo delay {:.1} ms", cached_delay as f32 / 48.0);
        self.delay_estimator.set_cached_delay(cached_delay as usize);
      }
    }

    // Echo strength remembered for this mic and output: AEC3's fallback echo estimate is right
    // from the first frame, until the strength is measured in this session.
    if cached_echo_x10 != self.cached_echo_seen {
      self.cached_echo_seen = cached_echo_x10;
      if cached_echo_x10 != 0 {
        self.aec.seed_echo_strength_db(cached_echo_x10 as f32 / 10.0);
      }
    }

    self.ai_denoiser.set_strength(ai_strength);
    self.ai_denoiser.set_gate_threshold_db(gate_db);
    // Never blocks: a heavy model not loaded yet loads on its own thread, RNNoise runs meanwhile.
    if model != self.ai_denoiser.selected_model() {
      self.ai_denoiser.request_model(model);
    }
    if self.denoise_retry_seen.is_some_and(|s| s != denoise_retry_seq) {
      self.ai_denoiser.retry();
    }
    self.denoise_retry_seen = Some(denoise_retry_seq);
    let model_before = self.ai_denoiser.active_model();
    if eq_version.is_some() && eq_version != self.eq_version {
      self.eq_version = eq_version;
      let bands = self.shm_header().map(|h| h.read_mic_eq()).unwrap_or_default();
      self.eq.set_bands(&bands, self.sample_rate);
    }

    self.mic_rms = smooth_rms(self.mic_rms, mic_in);
    // The converter clipped (only a lower input gain fixes that; the daemon decides).
    self.input_clips = self.input_clips.wrapping_add(self.clip_detector.count(mic_in));

    // Stage 1: Classical DSP Acoustic Echo Cancellation (NO AI)
    // AEC3 gets the mic at call level, by the speech level the leveler learned: at a quiet
    // mic's level it would duck the voice (`aec3.rs`, "Mic gain"). Nothing learned: no gain.
    let aec_mic_gain_db = self.leveler.gain_to_default_db().filter(|_| leveler_on).unwrap_or(0.0);
    self.aec.set_mic_gain_db(aec_mic_gain_db);
    self.aec_ref_offset = None;
    self.aec_state = if aec_enabled {
      self.run_aec(frames, mic_qpc_hns)
    } else {
      self.aec_idle = true; // switched back on: flush what it queued before
      AecState::Disabled
    };

    let Some(mic) = self.mic_mono_temp.get_mut(..frames) else {
      return;
    };
    #[cfg(feature = "debug-capture")]
    if let Some(cap) = self.capture.as_ref().filter(|_| capturing) {
      let [left, right] = &self.ref_stereo;
      for (stage, buffer) in
        [(STAGE_REF_AEC, &self.ref_buffer), (STAGE_REF_AEC_LEFT, left), (STAGE_REF_AEC_RIGHT, right)]
      {
        match buffer.get(..frames).filter(|_| self.aec_ref_offset.is_some()) {
          Some(refs) => cap.stage(stage, refs),
          None => cap.stage_silent(stage),
        }
      }
      cap.stage(STAGE_AFTER_AEC, mic);
    }

    // Stage 2: fixed low cut, then neural noise suppression (RNNoise, DeepFilterNet or DPDFNet).
    // The closed gate goes as far further down as the leveler lifts (last block's gain), so
    // pauses don't get louder with a quiet mic.
    self.pre_high_pass.process(mic);
    let leveler_boost_db = if leveler_on { self.leveler.gain_db() } else { 0.0 };
    self.ai_denoiser.set_gate_floor_extra_db(leveler_boost_db);
    self.ai_denoiser.set_pre_gain_db(leveler_boost_db);
    self.ai_denoiser.process_samples(mic);
    #[cfg(feature = "debug-capture")]
    if let Some(cap) = self.capture.as_ref().filter(|_| capturing) {
      cap.stage(STAGE_AFTER_DENOISE, mic);
    }

    // Stage 3: Parametric voice EQ
    if !self.eq.is_empty() {
      self.eq.process(mic);
    }
    #[cfg(feature = "debug-capture")]
    if let Some(cap) = self.capture.as_ref().filter(|_| capturing) {
      cap.stage(STAGE_AFTER_EQ, mic);
    }

    // Stage 4: Learning voice leveler (speech to one level; learns only while the detector hears speech)
    // "Learn again" (a new sequence number): forget, and don't take the old saved level back.
    if self.leveler_reset_seen.is_some_and(|s| s != leveler_reset_seq) {
      self.leveler.reset();
      self.leveler_seed_blocked = true;
    }
    self.leveler_reset_seen = Some(leveler_reset_seq);
    // A setting before the leveler moved (volume, denoiser, EQ, ...): relearn fast, gain held.
    let inputs = LevelerInputs {
      aec_enabled,
      ai_strength_permille: (ai_strength * 1000.0).round() as u32,
      gate_db: gate_db as i32,
      active_model: self.ai_denoiser.active_model().to_u32(),
      eq_version: self.eq_version,
    };
    if self.leveler_inputs.is_some_and(|prev| prev != inputs) {
      self.leveler.relearn();
    }
    self.leveler_inputs = Some(inputs);
    self.leveler.set_input_gain_db(input_gain_x10 as f32 / 10.0);
    self.leveler.set_target_db(leveler_target_x10 as f32 / 10.0);
    if leveler_on {
      // Level learned in an earlier stream on this mic (it may arrive a few blocks late).
      if leveler_saved_x10 != 0 && !self.leveler_seed_blocked {
        self.leveler.seed(leveler_saved_x10 as f32 / 10.0);
      }
      self.leveler.process(mic, self.ai_denoiser.heard_voice(), self.ai_denoiser.output_lead_samples());
    } else {
      self.leveler.reset();
    }
    #[cfg(feature = "debug-capture")]
    if let Some(cap) = self.capture.as_ref().filter(|_| capturing) {
      cap.stage(STAGE_AFTER_LEVELER, mic);
    }

    // Stage 5: volume + peak limiter, always last so EQ boosts can't clip. With the leveler the
    // mic volume (0..300 %) is how loud others hear you: 100 % = the target, 50 % = 6 dB under it
    // (the leveler took the Windows volume out of the input). Without it: only the boost > 100 %.
    let boost = if leveler_on || mic_permille > 1000 { mic_permille as f32 / 1000.0 } else { 1.0 };
    for sample in mic.iter_mut() {
      *sample *= boost;
    }
    self.limiter.process(mic, 1);
    self.chain.add_block(&self.ai_denoiser.debug_state(), self.ai_denoiser.heard_voice(), self.limiter.gain(), frames);
    // Field by field: `mic` still borrows `mic_mono_temp`.
    let stages = aec_latency_samples(self.aec_idle, self.aec_state, &self.aec) + self.ai_denoiser.output_lead_samples();
    if stages != std::mem::replace(&mut self.latency_stages, stages) {
      self.latency_meter.invalidate();
    }
    self.latency_meter.push_output(mic);

    self.out_rms = smooth_rms(self.out_rms, mic);
    #[cfg(feature = "debug-capture")]
    if let Some(cap) = self.capture.as_ref().filter(|_| capturing) {
      cap.stage(STAGE_OUT, mic);
    }

    // Output cleaned microphone audio
    for out in outputs.iter_mut() {
      let n = out.len().min(mic.len());
      if let (Some(o), Some(s)) = (out.get_mut(..n), mic.get(..n)) {
        o.copy_from_slice(s);
      }
    }

    let block_secs = frames as f32 / self.sample_rate.max(1.0);
    let load = started.elapsed().as_secs_f32() / block_secs;
    // The block a model takes over in sets up its scratch memory: not a sign of a slow CPU.
    if self.ai_denoiser.active_model() == model_before {
      self.load.add(load);
    }
    #[cfg(feature = "debug-capture")]
    if capturing {
      let settings = BlockSettings {
        aec_enabled,
        ai_strength,
        gate_db,
        leveler_on,
        leveler_target_db: leveler_target_x10 as f32 / 10.0,
        input_gain_db: input_gain_x10 as f32 / 10.0,
        mic_volume: mic_permille as f32 / 1000.0,
        boost,
        load,
      };
      self.capture_finish(frames, mic_qpc_hns, &settings);
    }
    self.block_counter = self.block_counter.wrapping_add(1);
    if self.block_counter.is_multiple_of(TELEMETRY_EVERY_BLOCKS) {
      if self.load.end_window(self.ai_denoiser.active_model().is_heavy()) {
        self.ai_denoiser.fall_back_overloaded();
      }
      if self.publishes {
        self.publish_telemetry();
      }
      self.load.reset_window();
    }
  }

  fn run_aec(&mut self, frames: usize, mic_qpc_hns: Option<u64>) -> AecState {
    // Assume it won't run; set back below when it does.
    let was_idle = std::mem::replace(&mut self.aec_idle, true);
    let Self {
      shm_client,
      delay_estimator,
      aec,
      ref_buffer,
      ref_stereo: [ref_left, ref_right],
      mic_mono_temp,
      ref_rms,
      aec_ref_offset,
      aec_idle,
      ..
    } = self;
    let Some(shm) = shm_client.as_mut() else {
      return AecState::NoReference;
    };
    let ring = shm.ring_buffer();
    let header = ring.header();

    let (Some(t), true) = (mic_qpc_hns, header.tap_active.load(Ordering::Relaxed)) else {
      delay_estimator.push_mic(&[], None);
      return AecState::NoReference;
    };
    let ref_now = header.ref_index_at(t);
    let stale = header.ref_age_hns(t).is_none_or(|age| age > REF_STALE_HNS);

    // Measure on the microphone before cancellation removes the echo: `mic_mono_temp`, not
    // `mic_in`, is NaN-free and clamped (one NaN would poison the correlation window).
    if delay_estimator.push_mic(mic_mono_temp.get(..frames).unwrap_or_default(), ref_now) {
      let (outcome, change) = delay_estimator.finish_window(|start, out| ring.read_frames_from(start, out));
      if change == LockChange::Changed {
        log::info!(
          "[AEC] echo delay locked at {:.1} ms ({:?})",
          delay_estimator.current_delay().unwrap_or(0) as f32 / 48.0,
          outcome
        );
        aec.reset();
      }
    }

    let Some(idx) = ref_now else {
      return AecState::NoReference;
    };
    let Some(delay) = delay_estimator.current_delay() else {
      return if stale { AecState::ReferenceSilent } else { AecState::Measuring };
    };

    let (Some(refs), Some(left), Some(right), Some(mic)) = (
      ref_buffer.get_mut(..frames),
      ref_left.get_mut(..frames),
      ref_right.get_mut(..frames),
      mic_mono_temp.get_mut(..frames),
    ) else {
      return AecState::NoReference;
    };
    let offset = delay.saturating_sub(PRE_DELAY_SAMPLES) as u32;
    ring.read_stereo_frames_from(idx.wrapping_sub(offset), left, right);
    for ((mix, &l), &r) in refs.iter_mut().zip(left.iter()).zip(right.iter()) {
      *mix = (l + r) * 0.5;
    }
    *aec_ref_offset = Some(offset);
    *ref_rms = smooth_rms(*ref_rms, refs);

    // Runs even when silent: it doesn't adapt then, but keeps its history continuous.
    if was_idle {
      aec.flush(); // the reference was gone: its queued output is from before the gap
    }
    aec.process_block_stereo(left, right, mic);
    *aec_idle = false;

    if stale || *ref_rms < REF_SILENT_RMS {
      AecState::ReferenceSilent
    } else {
      AecState::Locked
    }
  }

  fn publish_telemetry(&self) {
    let Some(h) = self.shm_header() else {
      return;
    };
    let erle_times_10 = (self.aec.erle_db() * 10.0).round() as i32;
    h.erle_db_times_10.store(erle_times_10, Ordering::Relaxed);
    h.aec_state.store(self.aec_state.to_u32(), Ordering::Relaxed);
    h.denoise_model_active.store(self.ai_denoiser.active_model().to_u32(), Ordering::Relaxed);
    h.denoise_limit.store(self.ai_denoiser.limit().to_u32(), Ordering::Relaxed);
    let permille = |x: f32| (x * 1000.0).round().clamp(0.0, 1e6) as u32;
    h.mic_dsp_load_peak_permille.store(permille(self.load.peak), Ordering::Relaxed);
    h.mic_dsp_load_avg_permille.store(permille(self.load.average()), Ordering::Relaxed);
    h.mic_dsp_overruns.store(self.load.late_total, Ordering::Relaxed);
    h.mic_leveler_gain_db_x10.store((self.leveler.gain_db() * 10.0).round() as i32, Ordering::Relaxed);
    let level_x10 = self.leveler.level_db().map_or(0, |l| ((l * 10.0).round() as i32).min(-1));
    h.mic_leveler_level_db_x10.store(level_x10, Ordering::Relaxed);
    h.mic_input_clips.store(self.input_clips, Ordering::Relaxed);
    h.mic_speech_ms.store(self.leveler.speech_ms(), Ordering::Relaxed);
    // Latency: measured end to end, plus what each stage holds right now (for the breakdown).
    let us = |samples: usize| (samples as u64 * 1_000_000 / 48_000) as u32;
    let measured = self.latency_meter.measured_samples().map_or(0, |s| us(s.round() as usize).max(1));
    h.mic_latency_measured_us.store(measured, Ordering::Relaxed);
    h.mic_latency_aec_us.store(us(aec_latency_samples(self.aec_idle, self.aec_state, &self.aec)), Ordering::Relaxed);
    let lookahead = self.ai_denoiser.lookahead_now_samples();
    let lead = self.ai_denoiser.output_lead_samples();
    h.mic_latency_denoise_us.store(us(lead.saturating_sub(lookahead)), Ordering::Relaxed);
    h.mic_latency_lookahead_us.store(us(lookahead), Ordering::Relaxed);
    // Chain state for the debug log.
    let dn = self.ai_denoiser.debug_state();
    let chain = &self.chain;
    h.mic_voice_blocks.store(chain.voice_blocks, Ordering::Relaxed);
    h.mic_blocks.store(chain.blocks, Ordering::Relaxed);
    h.mic_rescue_blocks.store(chain.rescue_blocks, Ordering::Relaxed);
    h.mic_in_prob_max_permille.store(permille(chain.last.in_prob), Ordering::Relaxed);
    h.mic_out_prob_max_permille.store(permille(chain.last.out_prob), Ordering::Relaxed);
    h.mic_soft_prob_max_permille.store(permille(chain.last.soft_prob), Ordering::Relaxed);
    h.mic_gate_gain_min_db_x10.store(db_x10(chain.last.gate_gain).min(0), Ordering::Relaxed);
    h.mic_limiter_gain_min_db_x10.store(db_x10(chain.last.limiter_gain).min(0), Ordering::Relaxed);
    h.mic_gate_floor_db_x10.store(if dn.gate_enabled { db_x10(dn.gate_floor) } else { 0 }, Ordering::Relaxed);
    // Not `db_x10`: it caps at +20 dB, the pre-gain goes to +24.
    h.mic_df_pre_gain_db_x10.store((200.0 * dn.df_pre_gain.max(1e-6).log10()).round() as i32, Ordering::Relaxed);
    let flag = |on: bool, bit: u32| if on { bit } else { 0 };
    let flags = flag(self.ai_denoiser.asleep(), MIC_CHAIN_ASLEEP)
      | flag(dn.bypassed, MIC_CHAIN_BYPASSED)
      | flag(dn.takeover, MIC_CHAIN_TAKEOVER)
      | flag(lookahead > 0, MIC_CHAIN_LOOKAHEAD)
      | flag(self.others, MIC_CHAIN_OTHERS)
      | flag(self.aec.is_stereo() && self.aec_state != AecState::Disabled, MIC_CHAIN_STEREO_REF);
    h.mic_chain_flags.store(flags, Ordering::Relaxed);
    h.mic_quiet_ms.store((dn.quiet_samples / 48).min(u32::MAX as usize) as u32, Ordering::Relaxed);

    let est = &self.delay_estimator;
    let to_us = |samples: f32| (samples / 48_000.0 * 1e6).round().max(0.0) as u32;
    h.aec_delay_us.store(est.current_delay().map_or(0, |d| to_us(d as f32)), Ordering::Relaxed);
    h.aec_delay_from_cache.store(est.is_from_cache(), Ordering::Relaxed);
    h.aec_estimate_count.store(est.estimate_count(), Ordering::Relaxed);
    let (raw, conf) = match est.last_outcome() {
      Some(WindowOutcome::Estimate { delay_samples, peak_ratio }) => (to_us(delay_samples), peak_ratio),
      Some(WindowOutcome::NoPeak { peak_ratio }) => (0, peak_ratio),
      _ => (0, 0.0),
    };
    h.aec_raw_delay_us.store(raw, Ordering::Relaxed);
    // Peak-to-average ratio from acceptance to full scale (10..40) mapped to 0..100 %.
    let span = FULL_SCALE_PEAK_RATIO - MIN_PEAK_RATIO;
    let conf_permille = (((conf - MIN_PEAK_RATIO) / span).clamp(0.0, 1.0) * 1000.0) as u32;
    h.aec_confidence_permille.store(conf_permille, Ordering::Relaxed);

    // 0 = no echo strength yet: a measured one is negative (-0.1 dB at most), a bound positive.
    let echo_x10 = self.aec.echo_strength_db().map_or(0, |(db, bound)| {
      let x10 = ((db * 10.0).round() as i32).clamp(-1200, -1);
      if bound {
        -x10
      } else {
        x10
      }
    });
    h.aec_echo_db_x10.store(echo_x10, Ordering::Relaxed);
    h.aec_fallback_echo_db_x10.store((self.aec.fallback_echo_db() * 10.0).round() as i32, Ordering::Relaxed);
    h.aec_mic_gain_db_x10.store((self.aec.mic_gain_db() * 10.0).round() as i32, Ordering::Relaxed);
    let (fit, slope) = self.aec.echo_fit();
    let signed_permille = |x: f32| (x * 1000.0).round().clamp(-10_000.0, 10_000.0) as i32;
    h.aec_echo_fit_permille.store(signed_permille(fit), Ordering::Relaxed);
    h.aec_echo_slope_permille.store(signed_permille(slope), Ordering::Relaxed);
    h.aec_double_talk.store(self.aec.is_double_talk(), Ordering::Relaxed);
    h.aec_ref_level_db_x10.store(db_x10(self.ref_rms), Ordering::Relaxed);
    h.aec_mic_level_db_x10.store(db_x10(self.mic_rms), Ordering::Relaxed);
    h.aec_out_level_db_x10.store(db_x10(self.out_rms), Ordering::Relaxed);
    h.aec_filter_taps.store(self.aec.taps() as u32, Ordering::Relaxed);
    h.aec_pre_delay_us.store(to_us(PRE_DELAY_SAMPLES as f32), Ordering::Relaxed);
    h.aec_corr_max_delay_us.store(to_us(DelayEstimator::max_delay_samples() as f32), Ordering::Relaxed);

    let mut ir = [0.0f32; AEC_IMPULSE_POINTS];
    self.aec.impulse_bins(&mut ir);
    for (slot, v) in h.aec_impulse.iter().zip(ir) {
      slot.store(v.to_bits(), Ordering::Relaxed);
    }
    for (slot, v) in h.aec_corr.iter().zip(est.corr_curve()) {
      slot.store(v.to_bits(), Ordering::Relaxed);
    }
    // Last, with Release: a daemon that sees this seq also sees the delay written above.
    h.aec_reference_seq_seen.store(self.reference_seq_seen, Ordering::Release);
    h.aec_telemetry_seq.fetch_add(1, Ordering::Release);
  }

  pub fn shm_header(&self) -> Option<&ShmHeader> {
    self.shm_client.as_ref().map(|c| &c.layout().header)
  }

  /// Starts a capture block if one is recording: raw mic + the reference at mic time.
  #[cfg(feature = "debug-capture")]
  fn capture_input(&mut self, mic_in: &[f32], mic_qpc_hns: Option<u64>) -> bool {
    let Self { capture, shm_client, .. } = self;
    let Some(cap) = capture.as_mut() else {
      return false;
    };
    if !cap.begin_block(mic_in.len(), mic_qpc_hns) {
      return false;
    }
    cap.stage(STAGE_MIC_RAW, mic_in);
    let ring = shm_client.as_mut().map(|s| s.ring_buffer());
    let idx = ring.as_ref().zip(mic_qpc_hns).and_then(|(r, t)| r.header().ref_index_at(t));
    let wrote = match (ring, idx, cap.scratch.get_mut(..mic_in.len())) {
      (Some(ring), Some(idx), Some(dst)) => {
        ring.read_frames_from(idx, dst);
        true
      }
      _ => false,
    };
    if wrote {
      cap.stage_scratch(STAGE_REF_AT_MIC);
    } else {
      cap.stage_silent(STAGE_REF_AT_MIC);
    }
    true
  }

  #[cfg(feature = "debug-capture")]
  fn capture_finish(&mut self, frames: usize, mic_qpc_hns: Option<u64>, s: &BlockSettings) {
    let Some(cap) = self.capture.as_ref() else {
      return;
    };
    let est = &self.delay_estimator;
    let b = |v: bool| if v { 1.0 } else { 0.0 };
    let (raw_delay, peak_ratio) = match est.last_outcome() {
      Some(WindowOutcome::Estimate { delay_samples, peak_ratio }) => (delay_samples, peak_ratio),
      Some(WindowOutcome::NoPeak { peak_ratio }) => (-1.0, peak_ratio),
      _ => (-1.0, 0.0),
    };
    let ref_age_ms =
      self.shm_header().zip(mic_qpc_hns).and_then(|(h, t)| h.ref_age_hns(t)).map_or(-1.0, |a| a as f32 / 10_000.0);
    let lin_db = |g: f32| (20.0 * g.log10()).max(-100.0);
    let dn = self.ai_denoiser.debug_state();
    let gate = |v: f32| if dn.gate_enabled { v } else { f32::NAN };
    let mic_auto =
      self.shm_header().map_or(f32::NAN, |h| h.mic_auto_gain_permille.load(Ordering::Relaxed) as f32 / 1000.0);
    let db = |rms: f32| db_x10(rms) as f32 / 10.0;
    // Positional: one value per `DEBUG_VARS` row, in that order (a misplaced value shifts every
    // column after it, as `load` did in the 2026-11-09 captures).
    let vars: [f32; DEBUG_VARS.len()] = [
      cap.frame() as f32,
      frames as f32,
      cap.t_ms(mic_qpc_hns),
      b(s.aec_enabled),
      self.aec_state.to_u32() as f32,
      est.current_delay().map_or(-1.0, |d| d as f32),
      b(est.is_from_cache()),
      raw_delay,
      peak_ratio,
      est.estimate_count() as f32,
      self.aec_ref_offset.map_or(-1.0, |o| o as f32),
      ref_age_ms,
      db(self.ref_rms),
      db(self.mic_rms),
      db(self.out_rms),
      self.aec.erle_db(),
      b(self.aec.is_double_talk()),
      self.aec.taps() as f32,
      self.ai_denoiser.selected_model().to_u32() as f32,
      self.ai_denoiser.active_model().to_u32() as f32,
      self.ai_denoiser.limit().to_u32() as f32,
      s.ai_strength,
      s.gate_db,
      b(self.ai_denoiser.heard_voice()),
      self.ai_denoiser.output_lead_samples() as f32,
      self.eq_version.map_or(-1.0, |v| v as f32),
      b(s.leveler_on),
      s.leveler_target_db,
      self.leveler.gain_db(),
      self.leveler.level_db().unwrap_or(f32::NAN),
      s.input_gain_db,
      s.mic_volume,
      s.boost,
      s.load,
      dn.in_prob,
      dn.out_prob,
      dn.rescue_prob,
      dn.soft_prob,
      dn.vad_run as f32,
      dn.vad_active_left as f32,
      dn.vad_out_active_left as f32,
      gate(lin_db(dn.gate_gain)),
      if dn.gate_enabled { dn.gate_hold_left as f32 } else { -1.0 },
      gate(lin_db(dn.gate_floor)),
      lin_db(dn.df_pre_gain),
      dn.rescue_mix,
      dn.rescue_hold_left as f32,
      b(self.ai_denoiser.asleep()),
      b(dn.bypassed),
      dn.quiet_samples as f32,
      b(dn.takeover),
      self.ai_denoiser.lookahead_now_samples() as f32,
      self.leveler.speech_ms() as f32,
      self.input_clips as f32,
      mic_auto,
      lin_db(self.limiter.gain()),
      b(self.aec.is_stereo()),
    ];
    if let Some(cap) = self.capture.as_mut() {
      cap.end_block(&vars);
    }
  }
}

/// Per-block settings the capture records next to the chain's state.
#[cfg(feature = "debug-capture")]
struct BlockSettings {
  aec_enabled: bool,
  ai_strength: f32,
  gate_db: f32,
  leveler_on: bool,
  leveler_target_db: f32,
  input_gain_db: f32,
  mic_volume: f32,
  boost: f32,
  load: f32,
}

impl Default for ChainExtremes {
  fn default() -> Self {
    // Probabilities of -1 (no frame finished) never beat 0; NaN never wins `max`/`min`.
    Self { in_prob: 0.0, out_prob: 0.0, soft_prob: 0.0, gate_gain: 1.0, limiter_gain: 1.0 }
  }
}

impl ChainWindow {
  fn add_block(&mut self, dn: &DenoiseDebug, heard_voice: bool, limiter_gain: f32, frames: usize) {
    self.blocks = self.blocks.wrapping_add(1);
    self.voice_blocks = self.voice_blocks.wrapping_add(u32::from(heard_voice));
    self.rescue_blocks = self.rescue_blocks.wrapping_add(u32::from(dn.rescue_mix > 0.5));
    let w = &mut self.now;
    w.in_prob = w.in_prob.max(dn.in_prob);
    w.out_prob = w.out_prob.max(dn.out_prob);
    w.soft_prob = w.soft_prob.max(dn.soft_prob);
    if dn.gate_enabled {
      w.gate_gain = w.gate_gain.min(dn.gate_gain);
    }
    w.limiter_gain = w.limiter_gain.min(limiter_gain);
    self.samples += frames;
    if self.samples >= CHAIN_WINDOW_SAMPLES {
      self.samples = 0;
      self.last = std::mem::take(&mut self.now);
    }
  }
}

impl DspLoad {
  /// `share`: processing time / block duration (1.0 = exactly real time).
  fn add(&mut self, share: f32) {
    self.peak = self.peak.max(share);
    self.sum += share;
    self.blocks += 1;
    if share > 1.0 {
      self.late_in_window += 1;
      self.late_total = self.late_total.wrapping_add(1);
    }
  }

  fn average(&self) -> f32 {
    if self.blocks == 0 {
      0.0
    } else {
      self.sum / self.blocks as f32
    }
  }

  /// True when the heavy model should give way (only counted while one runs).
  fn end_window(&mut self, deepfilter_active: bool) -> bool {
    let overloaded = self.late_in_window >= OVERLOAD_LATE_BLOCKS || self.average() > OVERLOAD_AVG_LOAD;
    self.overloaded_windows = if deepfilter_active && overloaded { self.overloaded_windows + 1 } else { 0 };
    if self.overloaded_windows >= OVERLOAD_WINDOWS_TO_FALL_BACK {
      self.overloaded_windows = 0;
      return true;
    }
    false
  }

  fn reset_window(&mut self) {
    self.peak = 0.0;
    self.sum = 0.0;
    self.blocks = 0;
    self.late_in_window = 0;
  }
}

fn smooth_rms(prev: f32, block: &[f32]) -> f32 {
  if block.is_empty() {
    return prev;
  }
  let rms = (block.iter().map(|x| x * x).sum::<f32>() / block.len() as f32).sqrt();
  let next = 0.7 * prev + 0.3 * rms;
  // A non-finite value would stick forever (0.7 * inf = inf): drop this block instead.
  match (next.is_finite(), prev.is_finite()) {
    (true, _) => next,
    (false, true) => prev,
    (false, false) => 0.0,
  }
}

/// dBFS x 10, limited to -120..+20 dB (an inf/NaN level must not saturate to i32::MAX).
fn db_x10(rms: f32) -> i32 {
  let rms = if rms.is_finite() { rms.clamp(1e-6, 10.0) } else { 1e-6 };
  (200.0 * rms.log10()).round() as i32
}

fn pre_high_pass() -> BiquadCascade {
  let mut hp = BiquadCascade::default();
  let band = BiquadBand {
    filter_type: FilterType::HighPass,
    frequency: PRE_HIGH_PASS_HZ,
    q: 0.707,
    slope_db: 12,
    ..BiquadBand::default()
  };
  hp.set_bands(&[band], 48_000.0);
  hp
}

/// Delay the echo canceller adds right now (0 while it doesn't run, idle or switched off).
fn aec_latency_samples(aec_idle: bool, aec_state: AecState, aec: &EchoCanceller3) -> usize {
  if !aec_idle && aec_state != AecState::Disabled {
    aec.latency_samples()
  } else {
    0
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use gamersequalizer_common::limiter::soft_limit;

  #[test]
  fn test_digital_boost_saturation() {
    let boost = 3.0; // 300% (+9.5 dB)
    let out: Vec<f32> = [0.1f32, 0.4, 0.8, 1.0].iter().map(|s| soft_limit(s * boost)).collect();

    // 0.1 * 3.0 = 0.3 (linear, unclipped)
    assert!((out[0] - 0.3).abs() < 1e-4);
    // 0.4 * 3.0 = 1.2 -> saturated smoothly <= 1.0
    assert!(out[1] > 0.95 && out[1] <= 1.0);
    // 1.0 * 3.0 = 3.0 -> smoothly saturated without wrapping
    assert!(out[3] > 0.95 && out[3] <= 1.0);
    assert!(soft_limit(-3.0) < -0.95 && soft_limit(-3.0) >= -1.0);
  }

  /// The whole mic chain (no AEC reference, default mic EQ) on the private recordings, for
  /// before/after comparisons of the stages after the denoiser. Per model and input level:
  /// output level of voice / whisper / room sections (dBFS, power mean), samples at full scale
  /// (>= 0.99, per mille), peak, leveler gain at the end; and keys.wav alone. Run:
  /// `cargo test -p gamersequalizer-mic-vst --release chain_report -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn chain_report() {
    use crate::ai_denoiser::tests::read_wav_48k;
    const HALF_SEC: usize = 24_000;
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test_data/private");
    if ["whisper_scenario.wav", "keys.wav"].iter().any(|f| !std::path::Path::new(dir).join(f).exists()) {
      return eprintln!("skipped: put whisper_scenario.wav and keys.wav into test_data/private/");
    }
    let scenario = read_wav_48k("../private/whisper_scenario.wav");
    let keys = read_wav_48k("../private/keys.wav");
    let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    let db = |p: f32| 10.0 * p.max(1e-12).log10();
    let pow = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32;
    // Default mic EQ of the app: low cut 100 Hz 48 dB/oct, high cut 9 kHz.
    let bands = [
      BiquadBand {
        filter_type: FilterType::HighPass,
        frequency: 100.0,
        q: 0.707,
        slope_db: 48,
        ..BiquadBand::default()
      },
      BiquadBand {
        filter_type: FilterType::LowPass,
        frequency: 9_000.0,
        q: 0.707,
        slope_db: 12,
        ..BiquadBand::default()
      },
    ];
    // (output, leveler gain at the end, share of blocks the heavy model slept, whisper section only)
    let run = |model: DenoiseModel, permille: u32, input: &[f32]| -> (Vec<f32>, f32, f32) {
      let mut p = GamersEqualizerPlugin::with_shm(48_000.0, None);
      p.test_overrides = Some((model, permille));
      p.ai_denoiser.set_model(model);
      p.eq.set_bands(&bands, 48_000.0);
      let mut out = vec![0.0f32; input.len()];
      let mut asleep = 0usize;
      let whisper_blocks = 40 * HALF_SEC / 480..48 * HALF_SEC / 480;
      for (n, (i, o)) in input.chunks(480).zip(out.chunks_mut(480)).enumerate() {
        p.process(&[i], &mut [o]);
        asleep += usize::from(whisper_blocks.contains(&n) && p.ai_denoiser.asleep());
      }
      let lead = p.ai_denoiser.output_lead_samples();
      let slept = asleep as f32 * 100.0 / whisper_blocks.len() as f32;
      (out.get(lead..).unwrap_or_default().to_vec(), p.leveler.gain_db(), slept)
    };
    let scale = |x: &[f32], g_db: f32| -> Vec<f32> { x.iter().map(|s| s * 10f32.powf(g_db / 20.0)).collect() };
    // Desk rumble: 45 Hz at -30 dBFS, swelling at 0.3 Hz.
    let rumble: Vec<f32> = scenario
      .iter()
      .enumerate()
      .map(|(i, s)| {
        let t = i as f32 / 48_000.0;
        s + 0.0316 * (0.5 + 0.5 * (std::f32::consts::TAU * 0.3 * t).sin()) * (std::f32::consts::TAU * 45.0 * t).sin()
      })
      .collect();
    let hot_db = db(0.89f32.powi(2) / peak(&scenario).max(1e-6).powi(2));
    // A mic set far too hot: voice ~-10 dBFS, the converter clips the peaks.
    let clipped_in: Vec<f32> = scale(&scenario, 20.0).iter().map(|s| s.clamp(-1.0, 1.0)).collect();
    let variants: [(&str, Vec<f32>, u32); 8] = [
      ("-24 dB", scale(&scenario, -24.0), 1000),
      ("-12 dB", scale(&scenario, -12.0), 1000),
      ("0 dB", scenario.clone(), 1000),
      ("hot -1 dBFS pk", scale(&scenario, hot_db), 1000),
      ("+20 dB clipped", clipped_in, 1000),
      ("0 dB boost 200%", scenario.clone(), 2000),
      ("0 dB boost 300%", scenario.clone(), 3000),
      ("0 dB + rumble", rumble, 1000),
    ];
    // Pauses once the leveler has learned: 10th percentile of the 10 ms frame levels after 30 s
    // (digital silence left out).
    let pauses = |x: &[f32]| {
      let mut l: Vec<f32> =
        x.get(60 * HALF_SEC..).unwrap_or_default().chunks(480).map(|f| db(pow(f))).filter(|l| *l > -119.0).collect();
      l.sort_by(f32::total_cmp);
      l.get(l.len() / 10).copied().unwrap_or(-120.0)
    };
    let sec = |x: &[f32], a: usize, b: usize| db(pow(x.get(a * HALF_SEC..b * HALF_SEC).unwrap_or_default()));
    eprintln!(
      "input {:+.1} dB = hot; sections dBFS: voice 45-49 s | voice 53-57 s | whisper 20-24 s | room 1-18 s",
      hot_db
    );
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::RnNoise] {
      for (name, input, permille) in &variants {
        let (out, gain, slept) = run(model, *permille, input);
        let full = out.iter().filter(|s| s.abs() >= 0.99).count() as f32 * 1000.0 / out.len().max(1) as f32;
        eprintln!(
          "{model:<13?} {name:<16} voice {:6.1} {:6.1} whisper {:6.1} room {:6.1} pauses {:6.1} | full scale {:6.2} permille, peak {:5.2} dBFS | leveler {:+5.1} dB | asleep in whisper {:3.0} %",
          sec(&out, 90, 98),
          sec(&out, 107, 115),
          sec(&out, 40, 48),
          sec(&out, 2, 36),
          pauses(&out),
          full,
          20.0 * peak(&out).max(1e-6).log10(),
          gain,
          slept
        );
      }
      let (out, _, _) = run(model, 1000, &keys);
      eprintln!("{model:<13?} keys.wav alone: in {:6.1} dBFS, out {:6.1} dBFS", db(pow(&keys)), db(pow(&out)));
    }
  }

  #[test]
  fn test_db_x10() {
    assert_eq!(db_x10(1.0), 0);
    assert_eq!(db_x10(0.1), -200);
    assert_eq!(db_x10(0.0), -1200);
    assert_eq!(db_x10(f32::INFINITY), -1200);
    assert_eq!(db_x10(f32::NAN), -1200);
    assert_eq!(db_x10(1e30), 200);
  }

  #[test]
  fn test_smooth_rms_recovers_from_non_finite_block() {
    // Regression: one inf sample pinned the speaker meter at "214748364.7" for good.
    let rms = smooth_rms(0.1, &[f32::INFINITY, 0.0]);
    assert_eq!(rms, 0.1);
    assert_eq!(smooth_rms(f32::INFINITY, &[f32::NAN]), 0.0);
    assert!((smooth_rms(rms, &[0.1; 4]) - 0.1).abs() < 1e-6);
  }

  #[test]
  fn test_overload_needs_consecutive_windows_while_deepfilter_runs() {
    let window = |load: &mut DspLoad, share: f32, df: bool| {
      for _ in 0..TELEMETRY_EVERY_BLOCKS {
        load.add(share);
      }
      let fall_back = load.end_window(df);
      load.reset_window();
      fall_back
    };
    let mut load = DspLoad::default();
    // RNNoise slow: reported (late blocks) but nothing to fall back from.
    assert!(!(0..5).any(|_| window(&mut load, 1.2, false)));
    assert_eq!(load.late_total, 5 * TELEMETRY_EVERY_BLOCKS);
    // A short spike is tolerated, a sustained overload is not.
    assert!(!window(&mut load, 1.5, true));
    assert!(!window(&mut load, 0.1, true));
    assert!(!window(&mut load, 0.95, true));
    assert!(!window(&mut load, 0.95, true));
    assert!(window(&mut load, 0.95, true));
  }

  /// Regression: the delay estimator got the raw input, so one NaN sample made a whole 1 s
  /// window read as "no echo" instead of measuring the echo in it.
  #[test]
  fn test_nan_in_the_mic_does_not_spoil_the_delay_window() {
    const ECHO_DELAY: usize = 2_000;
    let reference = noise(120_000, 1);
    let mut p = plugin_with_reference("nan_delay", &reference);
    let mut out = vec![0.0f32; BLOCK];
    let mut k = 0;
    while p.delay_estimator.last_outcome().is_none() {
      assert!(k < 200, "no measurement window finished");
      let t = block_hns(k);
      let start = ref_position(&p, t, reference.len());
      let mut mic: Vec<f32> =
        (0..BLOCK).map(|i| 0.3 * (start + i).checked_sub(ECHO_DELAY).map_or(0.0, |j| reference[j])).collect();
      if k == 3 {
        mic[100] = f32::NAN;
      }
      p.process_timed(&[&mic], &mut [&mut out], Some(t));
      k += 1;
    }
    let outcome = p.delay_estimator.last_outcome();
    assert!(matches!(outcome, Some(WindowOutcome::Estimate { .. })), "{outcome:?}");
    assert!(p.delay_estimator.corr_curve().iter().all(|v| v.is_finite()));
  }

  /// Regression: AEC off -> on replayed the samples AEC3 had queued before it was switched off.
  #[test]
  fn test_aec_switched_back_on_drops_audio_queued_before() {
    let mut p = plugin_with_reference("aec_idle", &noise(120_000, 2));
    p.shm_header().expect("mapping").aec_cached_delay_samples.store(960, Ordering::Relaxed); // AEC runs at once
    let (loud, mut out) = (noise(BLOCK, 3), vec![0.0f32; BLOCK]);
    for k in 0..20 {
      p.process_timed(&[&loud], &mut [&mut out], Some(block_hns(k)));
    }
    assert!(!p.aec_idle, "the AEC ran ({:?})", p.aec_state);
    p.shm_header().expect("mapping").aec_enabled.store(false, Ordering::Relaxed);
    p.process_timed(&[&loud], &mut [&mut out], Some(block_hns(20)));
    p.shm_header().expect("mapping").aec_enabled.store(true, Ordering::Relaxed);
    // The AEC stage alone (later stages keep their own history): silence in, silence out.
    p.mic_mono_temp[..BLOCK].fill(0.0);
    p.run_aec(BLOCK, Some(block_hns(21)));
    assert!(p.mic_mono_temp[..BLOCK].iter().all(|&s| s == 0.0), "samples from before the switch came out");
  }

  /// Blocks that don't line up with AEC3's 480-sample frames, so it queues output.
  const BLOCK: usize = 441;
  const T0_HNS: u64 = 10_000_000_000;

  /// Plugin on a private mapping whose loopback reference played `reference` from `T0_HNS`;
  /// RNNoise, 100 % volume.
  fn plugin_with_reference(tag: &str, reference: &[f32]) -> GamersEqualizerPlugin {
    let name = format!("Local\\GamersEqualizer_test_{tag}_{}", std::process::id());
    let mut shm = SharedMemoryClient::create_named(&name, None).expect("test mapping");
    let frames: Vec<StereoFrame> = reference.iter().map(|&s| StereoFrame { left: s, right: s }).collect();
    shm.ring_buffer().push_frames_timed(&frames, T0_HNS);
    shm.layout().header.tap_active.store(true, Ordering::Relaxed); // the loopback writer's flag
    let mut p = GamersEqualizerPlugin::with_shm(48_000.0, Some(shm));
    p.test_overrides = Some((DenoiseModel::RnNoise, 1000));
    p
  }

  /// QPC time of mic block `k`, from 0.5 s into the reference.
  fn block_hns(k: usize) -> u64 {
    T0_HNS + 5_000_000 + (k * BLOCK) as u64 * 10_000_000 / 48_000
  }

  /// Position in the reference (`len` samples, all written) that played at `t`.
  fn ref_position(p: &GamersEqualizerPlugin, t: u64, len: usize) -> usize {
    let h = p.shm_header().expect("test mapping");
    let first = h.ref_snap_index.load(Ordering::Relaxed).wrapping_sub(len as u32);
    h.ref_index_at(t).expect("reference").wrapping_sub(first) as usize
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
