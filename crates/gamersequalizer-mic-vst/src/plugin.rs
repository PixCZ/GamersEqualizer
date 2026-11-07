use crate::aec::{AcousticEchoCanceller, PRE_DELAY_SAMPLES};
use crate::ai_denoiser::AiVoiceIsolator;
use crate::clip_detector::ClipDetector;
use crate::delay_estimator::{DelayEstimator, LockChange, WindowOutcome, FULL_SCALE_PEAK_RATIO, MIN_PEAK_RATIO};
use crate::shared_memory::SharedMemoryClient;
use crate::voice_leveler::VoiceLeveler;
use gamersequalizer_common::biquad::BiquadCascade;
use gamersequalizer_common::limiter::soft_limit;
use gamersequalizer_common::shm::{
  AecState, DenoiseModel, ShmHeader, StereoFrame, AEC_IMPULSE_POINTS, MIC_LEVELER_DEFAULT_TARGET_DB_X10,
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
/// Soft limiter knee; transparent below.

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
  aec: AcousticEchoCanceller,
  ai_denoiser: AiVoiceIsolator,
  leveler: VoiceLeveler,
  /// Last "Learn again" sequence seen (None before the first block).
  leveler_reset_seen: Option<u32>,
  /// After "Learn again" in this stream, the saved level is stale until the daemon clears it.
  leveler_seed_blocked: bool,
  /// Settings before the leveler (they change the speech level it sees), last block.
  leveler_inputs: Option<LevelerInputs>,
  eq: BiquadCascade,
  eq_version: Option<u32>,
  pub sample_rate: f32,
  ref_buffer: Vec<f32>,
  mic_mono_temp: Vec<f32>,
  tap_frames: Vec<StereoFrame>,
  aec_state: AecState,
  /// Last `aec_cached_delay_samples` seen in shared memory (warm start when it changes).
  cached_delay_seen: u32,
  /// Last `aec_reference_seq` seen: when it changes, the speaker output was switched.
  reference_seq_seen: u32,
  block_counter: u32,
  ref_rms: f32,
  mic_rms: f32,
  out_rms: f32,
  load: DspLoad,
  /// Clipped raw input samples (full scale or flat tops), all time (wraps).
  input_clips: u32,
  clip_detector: ClipDetector,
  /// Shift of the reference fed to the AEC in the last block (None: the AEC didn't run).
  aec_ref_offset: Option<u32>,
  #[cfg(feature = "debug-capture")]
  capture: Option<CaptureWriter>,
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
      aec: AcousticEchoCanceller::default_48k(),
      ai_denoiser: AiVoiceIsolator::new(),
      leveler: VoiceLeveler::new(),
      leveler_reset_seen: None,
      leveler_seed_blocked: false,
      leveler_inputs: None,
      eq: BiquadCascade::default(),
      eq_version: None,
      sample_rate,
      ref_buffer: vec![0.0; 2048],
      mic_mono_temp: vec![0.0; 2048],
      tap_frames: Vec::with_capacity(2048),
      aec_state: AecState::Disabled,
      cached_delay_seen: 0,
      reference_seq_seen: 0,
      block_counter: 0,
      ref_rms: 0.0,
      mic_rms: 0.0,
      out_rms: 0.0,
      load: DspLoad::default(),
      input_clips: 0,
      clip_detector: ClipDetector::default(),
      aec_ref_offset: None,
      #[cfg(feature = "debug-capture")]
      capture: None,
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

  fn mic_process(&mut self, mic_in: &[f32], outputs: &mut [&mut [f32]], mic_qpc_hns: Option<u64>) {
    // Master switch "everything off": the raw microphone, no AEC/denoise/EQ/boost.
    if self.shm_header().is_some_and(|h| h.master_bypass.load(Ordering::Relaxed)) {
      for out in outputs.iter_mut() {
        let n = out.len().min(mic_in.len());
        if let (Some(o), Some(s)) = (out.get_mut(..n), mic_in.get(..n)) {
          o.copy_from_slice(s);
        }
      }
      return;
    }
    let started = std::time::Instant::now();
    let frames = mic_in.len();
    if self.mic_mono_temp.len() < frames {
      self.mic_mono_temp.resize(frames, 0.0);
    }
    if self.ref_buffer.len() < frames {
      self.ref_buffer.resize(frames, 0.0);
    }
    if let Some(dst) = self.mic_mono_temp.get_mut(..frames) {
      dst.copy_from_slice(mic_in);
    }
    #[cfg(feature = "debug-capture")]
    let capturing = self.capture_input(mic_in, mic_qpc_hns);

    // Fetch control parameters from shared memory
    let mut aec_enabled = true;
    let mut ai_strength = 0.75f32;
    let mut gate_db = NOISE_GATE_AUTO_LOOKAHEAD as f32; // the default until settings are read
    let mut mic_permille = 1000u32;
    let mut model = DenoiseModel::RnNoise;
    let mut eq_version = None;
    let mut cached_delay = 0u32;
    let mut reference_seq = self.reference_seq_seen;
    let mut leveler_on = true;
    let mut input_gain_x10 = 0;
    let (mut leveler_target_x10, mut leveler_reset_seq, mut leveler_saved_x10) = (MIC_LEVELER_DEFAULT_TARGET_DB_X10, 0, 0);
    if let Some(header) = self.shm_header() {
      leveler_on = header.mic_leveler_enabled.load(Ordering::Relaxed);
      input_gain_x10 = header.mic_input_gain_db_x10.load(Ordering::Relaxed);
      leveler_target_x10 = header.mic_leveler_target_db_x10.load(Ordering::Relaxed);
      leveler_reset_seq = header.mic_leveler_reset_seq.load(Ordering::Relaxed);
      leveler_saved_x10 = header.mic_leveler_saved_level_db_x10.load(Ordering::Relaxed);
      // Acquire before the cached delay: the daemon stores the new output's delay first.
      reference_seq = header.aec_reference_seq.load(Ordering::Acquire);
      cached_delay = header.aec_cached_delay_samples.load(Ordering::Relaxed);
      aec_enabled = header.aec_enabled.load(Ordering::Relaxed);
      ai_strength = header.ai_voice_strength_permille.load(Ordering::Relaxed) as f32 / 1000.0;
      gate_db = header.noise_gate_threshold_db.load(Ordering::Relaxed) as f32;
      let p = header.mic_volume_permille.load(Ordering::Relaxed);
      if p > 0 {
        mic_permille = p;
      }
      model = DenoiseModel::from_u32(header.denoise_model.load(Ordering::Relaxed));
      eq_version = Some(header.mic_eq_version.load(Ordering::Acquire));
    }

    // Speaker output switched: the locked delay and the learned echo path are the old
    // device's. Start over, warm-started below with the delay remembered for the new one.
    if reference_seq != self.reference_seq_seen {
      self.reference_seq_seen = reference_seq;
      log::info!("[AEC] speaker output changed: echo delay and filter start over");
      self.delay_estimator.forget_lock();
      self.aec.reset();
      self.cached_delay_seen = 0;
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

    self.ai_denoiser.set_strength(ai_strength);
    self.ai_denoiser.set_gate_threshold_db(gate_db);
    // Loading a heavy model blocks this call once (0.1-0.2 s): not a sign of a slow CPU.
    let loads_model = model != self.ai_denoiser.selected_model();
    if loads_model {
      self.ai_denoiser.set_model(model);
    }
    if eq_version.is_some() && eq_version != self.eq_version {
      self.eq_version = eq_version;
      let bands = self.shm_header().map(|h| h.read_mic_eq()).unwrap_or_default();
      self.eq.set_bands(&bands, self.sample_rate);
    }

    self.mic_rms = smooth_rms(self.mic_rms, mic_in);
    // The converter clipped (only a lower input gain fixes that; the daemon decides).
    self.input_clips = self.input_clips.wrapping_add(self.clip_detector.count(mic_in));

    // Stage 1: Classical DSP Acoustic Echo Cancellation (NO AI)
    self.aec_ref_offset = None;
    self.aec_state = if aec_enabled { self.run_aec(frames, mic_in, mic_qpc_hns) } else { AecState::Disabled };

    let Some(mic) = self.mic_mono_temp.get_mut(..frames) else {
      return;
    };
    #[cfg(feature = "debug-capture")]
    if let Some(cap) = self.capture.as_ref().filter(|_| capturing) {
      match self.ref_buffer.get(..frames).filter(|_| self.aec_ref_offset.is_some()) {
        Some(refs) => cap.stage(STAGE_REF_AEC, refs),
        None => cap.stage_silent(STAGE_REF_AEC),
      }
      cap.stage(STAGE_AFTER_AEC, mic);
    }

    // Stage 2: Neural noise suppression (RNNoise, DeepFilterNet or DPDFNet)
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

    // Stage 5: volume + soft limiter, always last so EQ boosts can't clip. With the leveler the
    // mic volume (0..300 %) is how loud others hear you: 100 % = the target, 50 % = 6 dB under it
    // (the leveler took the Windows volume out of the input). Without it: only the boost > 100 %.
    let boost = if leveler_on || mic_permille > 1000 { mic_permille as f32 / 1000.0 } else { 1.0 };
    for sample in mic.iter_mut() {
      *sample = soft_limit(*sample * boost);
    }

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
    if !loads_model {
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
      self.publish_telemetry();
      self.load.reset_window();
    }
  }

  fn run_aec(&mut self, frames: usize, mic_in: &[f32], mic_qpc_hns: Option<u64>) -> AecState {
    let Self { shm_client, delay_estimator, aec, ref_buffer, mic_mono_temp, ref_rms, aec_ref_offset, .. } = self;
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

    // Measure on the raw microphone (before cancellation removes the echo).
    if delay_estimator.push_mic(mic_in, ref_now) {
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

    let (Some(refs), Some(mic)) = (ref_buffer.get_mut(..frames), mic_mono_temp.get_mut(..frames)) else {
      return AecState::NoReference;
    };
    let offset = delay.saturating_sub(PRE_DELAY_SAMPLES) as u32;
    ring.read_frames_from(idx.wrapping_sub(offset), refs);
    *aec_ref_offset = Some(offset);
    *ref_rms = smooth_rms(*ref_rms, refs);

    // Runs even when silent: it doesn't adapt then, but keeps its history continuous.
    aec.process_block(refs, mic);

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
    let ref_age_ms = self
      .shm_header()
      .zip(mic_qpc_hns)
      .and_then(|(h, t)| h.ref_age_hns(t))
      .map_or(-1.0, |a| a as f32 / 10_000.0);
    let db = |rms: f32| db_x10(rms) as f32 / 10.0;
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

#[cfg(test)]
mod tests {
  use super::*;

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
}
