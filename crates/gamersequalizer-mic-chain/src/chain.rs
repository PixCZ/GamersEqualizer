use crate::aec3::{EchoCanceller3, PRE_DELAY_SAMPLES};
use crate::ai_denoiser::{AiVoiceIsolator, DenoiseDebug};
use crate::clip_detector::ClipDetector;
use crate::delay_estimator::{DelayEstimator, FULL_SCALE_PEAK_RATIO, LockChange, MIN_PEAK_RATIO, WindowOutcome};
use crate::latency_meter::LatencyMeter;
use crate::shared_memory::SharedMemoryClient;
use crate::voice_leveler::VoiceLeveler;
use gamersequalizer_common::biquad::BiquadCascade;
use gamersequalizer_common::limiter::PeakLimiter;
use gamersequalizer_common::profile::{BiquadBand, FilterType};
use gamersequalizer_common::shm::{
  AecState, DenoiseModel, MIC_CHAIN_ASLEEP, MIC_CHAIN_BYPASSED, MIC_CHAIN_LOOKAHEAD, MIC_CHAIN_OTHERS,
  MIC_CHAIN_STEREO_REF, MIC_CHAIN_TAKEOVER, MIC_LEVELER_DEFAULT_TARGET_DB_X10, NOISE_GATE_AUTO_LOOKAHEAD, ShmHeader,
  ShmLayout,
};
use std::sync::atomic::Ordering;

#[cfg(feature = "debug-capture")]
use crate::debug_capture::{CaptureWriter, DebugCaptureMap};
#[cfg(feature = "debug-capture")]
use gamersequalizer_common::debug_capture::*;

#[cfg(feature = "debug-capture")]
mod capture;
mod settings;
mod telemetry;

use settings::*;
use telemetry::*;

/// Publish telemetry every 10 blocks (~100 ms): enough for a smooth UI, negligible cost.
const TELEMETRY_EVERY_BLOCKS: u32 = 10;
/// Reference older than 200 ms means nothing is playing (no block was written).
const REF_STALE_HNS: u64 = 2_000_000;
/// Below this RMS the reference counts as silent for the UI state.
const REF_SILENT_RMS: f32 = 1e-4;

/// Fixed low cut before the denoiser: 2nd-order Butterworth, below any voice's fundamental.
const PRE_HIGH_PASS_HZ: f32 = 70.0;

/// The chain runs at 48 kHz only; the APO bridges other device rates (`RateBridge`).
const SAMPLE_RATE: f32 = 48_000.0;

pub struct MicChain {
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
  /// Speaker reference of the block as the AEC gets it: mono mix, and left / right.
  ref_buffer: Vec<f32>,
  ref_stereo: [Vec<f32>; 2],
  mic_mono_temp: Vec<f32>,
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
  /// Volume gain of the last block: a change ramps over one block instead of jumping. None: the
  /// next block starts at its own gain (new stream, after a pause).
  boost_gain: Option<f32>,
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

impl MicChain {
  /// Uses the given mapping for settings, reference audio and telemetry (the APO passes
  /// its Global mapping here).
  pub fn with_shm(shm_client: Option<SharedMemoryClient>) -> Self {
    Self {
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
      limiter: PeakLimiter::new(SAMPLE_RATE),
      ref_buffer: vec![0.0; 2048],
      ref_stereo: [vec![0.0; 2048], vec![0.0; 2048]],
      mic_mono_temp: vec![0.0; 2048],
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
      boost_gain: None,
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

  /// Apply the noise model chosen in the UI now (a heavy model loads here, 0.1-0.2 s).
  /// Call before audio starts so the first selection doesn't cause a gap.
  pub fn preload_selected_model(&mut self) {
    if let Some(model) = self.shm_header().map(|h| DenoiseModel::from_u32(h.denoise_model.load(Ordering::Relaxed))) {
      self.ai_denoiser.set_model(model);
    }
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
    self.mic_process(first, outputs, mic_qpc_hns);
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
      self.boost_gain = None;
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

    #[cfg_attr(not(test), expect(unused_mut, reason = "only the tests change it"))]
    let mut s = MicSettings::read(self.shm_header());
    #[cfg(test)]
    if let Some((model, permille)) = self.test_overrides {
      (s.model, s.mic_permille) = (model, permille);
    }

    // Speaker output switched: the locked delay and the learned echo path are the old
    // device's. Start over, warm-started below with the delay remembered for the new one.
    if let Some(reference_seq) = s.reference_seq.filter(|&seq| seq != self.reference_seq_seen) {
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
    if s.cached_delay != self.cached_delay_seen {
      self.cached_delay_seen = s.cached_delay;
      if s.cached_delay > 0 && self.delay_estimator.current_delay().is_none() {
        log::info!("[AEC] warm start with cached echo delay {:.1} ms", s.cached_delay as f32 / 48.0);
        self.delay_estimator.set_cached_delay(s.cached_delay as usize);
      }
    }

    // Echo strength remembered for this mic and output: AEC3's fallback echo estimate is right
    // from the first frame, until the strength is measured in this session.
    if s.cached_echo_x10 != self.cached_echo_seen {
      self.cached_echo_seen = s.cached_echo_x10;
      if s.cached_echo_x10 != 0 {
        self.aec.seed_echo_strength_db(s.cached_echo_x10 as f32 / 10.0);
      }
    }

    self.ai_denoiser.set_strength(s.ai_strength_permille as f32 / 1000.0);
    self.ai_denoiser.set_gate_threshold_db(s.gate_db as f32);
    self.ai_denoiser.set_voice_wanted(s.leveler_on);
    // Never blocks: a heavy model not loaded yet loads on its own thread, RNNoise runs meanwhile.
    if s.model != self.ai_denoiser.selected_model() {
      self.ai_denoiser.request_model(s.model);
    }
    if self.denoise_retry_seen.is_some_and(|seen| seen != s.denoise_retry_seq) {
      self.ai_denoiser.retry();
    }
    self.denoise_retry_seen = Some(s.denoise_retry_seq);
    let model_before = self.ai_denoiser.active_model();
    if s.eq_version.is_some() && s.eq_version != self.eq_version {
      self.eq_version = s.eq_version;
      // The field, not `shm_header()`: that would borrow all of `self` while `eq` changes.
      let bands = self.shm_client.as_ref().map(|c| c.layout().header.read_mic_eq());
      self.eq.set_bands(bands.into_iter().flatten(), SAMPLE_RATE);
    }

    self.mic_rms = smooth_rms(self.mic_rms, mic_in);
    // The converter clipped (only a lower input gain fixes that; the daemon decides).
    self.input_clips = self.input_clips.wrapping_add(self.clip_detector.count(mic_in));

    // Stage 1: Classical DSP Acoustic Echo Cancellation (NO AI)
    // AEC3 gets the mic at call level, by the speech level the leveler learned: at a quiet
    // mic's level it would duck the voice (`aec3.rs`, "Mic gain"). Nothing learned: no gain.
    let aec_mic_gain_db = self.leveler.gain_to_default_db().filter(|_| s.leveler_on).unwrap_or(0.0);
    self.aec.set_mic_gain_db(aec_mic_gain_db);
    self.aec_ref_offset = None;
    self.aec_state = if s.aec_enabled {
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
    let leveler_boost_db = if s.leveler_on { self.leveler.gain_db() } else { 0.0 };
    self.ai_denoiser.set_gate_floor_extra_db(leveler_boost_db);
    self.ai_denoiser.set_pre_gain_db(leveler_boost_db);
    self.ai_denoiser.set_aec_active(self.aec_state == AecState::Locked);
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
    if self.leveler_reset_seen.is_some_and(|seen| seen != s.leveler_reset_seq) {
      self.leveler.reset();
      self.leveler_seed_blocked = true;
    }
    self.leveler_reset_seen = Some(s.leveler_reset_seq);
    // The daemon cleared the stale level: what it saves next was learned since.
    if s.leveler_saved_x10 == 0 {
      self.leveler_seed_blocked = false;
    }
    // A setting before the leveler moved (volume, denoiser, EQ, ...): relearn fast, gain held.
    let inputs = LevelerInputs {
      aec_enabled: s.aec_enabled,
      ai_strength_permille: s.ai_strength_permille,
      gate_db: s.gate_db,
      active_model: self.ai_denoiser.active_model().to_u32(),
      eq_version: self.eq_version,
    };
    if self.leveler_inputs.is_some_and(|prev| prev != inputs) {
      self.leveler.relearn();
    }
    self.leveler_inputs = Some(inputs);
    self.leveler.set_input_gain_db(s.input_gain_x10 as f32 / 10.0);
    self.leveler.set_target_db(s.leveler_target_x10 as f32 / 10.0);
    if s.leveler_on {
      // Level learned in an earlier stream on this mic (it may arrive a few blocks late).
      if s.leveler_saved_x10 != 0 && !self.leveler_seed_blocked {
        self.leveler.seed(s.leveler_saved_x10 as f32 / 10.0);
      }
      // After the seed: a listed app's hold starts on the level remembered for this voice.
      self.leveler.set_hold(s.leveler_hold);
      self.leveler.process(mic, self.ai_denoiser.heard_level_voice(), self.ai_denoiser.output_lead_samples());
    } else {
      self.leveler.reset();
    }
    #[cfg(feature = "debug-capture")]
    if let Some(cap) = self.capture.as_ref().filter(|_| capturing) {
      cap.stage(STAGE_AFTER_LEVELER, mic);
    }

    // Stage 5: volume + peak limiter, always last so EQ boosts can't clip. With the leveler its
    // target alone is how loud others hear you: the mic volume only sets the input (the leveler
    // takes it out), so a slider moved by hand, by an app or by the tray changes nothing for them.
    // Without it: only the boost > 100 % (up to 300 %).
    let boost = if !s.leveler_on && s.mic_permille > 1000 { s.mic_permille as f32 / 1000.0 } else { 1.0 };
    let mut gain = self.boost_gain.replace(boost).unwrap_or(boost);
    if (gain - boost).abs() > 1e-6 && !mic.is_empty() {
      let step = (boost - gain) / mic.len() as f32;
      for sample in mic.iter_mut() {
        gain += step;
        *sample *= gain;
      }
    } else if boost != 1.0 {
      for sample in mic.iter_mut() {
        *sample *= boost;
      }
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

    let block_secs = frames as f32 / SAMPLE_RATE;
    let load = started.elapsed().as_secs_f32() / block_secs;
    // The block a model takes over in sets up its scratch memory: not a sign of a slow CPU.
    if self.ai_denoiser.active_model() == model_before {
      self.load.add(load);
    }
    #[cfg(feature = "debug-capture")]
    if capturing {
      self.capture_finish(frames, mic_qpc_hns, &s, boost, load);
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

    // A source exists (`tap_active`) but has written nothing yet: nothing has played.
    let Some(idx) = ref_now else {
      return AecState::ReferenceSilent;
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

    if stale || *ref_rms < REF_SILENT_RMS { AecState::ReferenceSilent } else { AecState::Locked }
  }

  /// Gain the voice leveler applies now, dB (0 while it is off or still learning). For the
  /// sidetone tap of the same APO.
  pub fn leveler_gain_db(&self) -> f32 {
    self.leveler.gain_db()
  }

  pub fn shm_header(&self) -> Option<&ShmHeader> {
    self.shm_client.as_ref().map(|c| &c.layout().header)
  }

  /// The whole mapping (the sidetone ring lives beside the header).
  pub fn shm_layout(&self) -> Option<&ShmLayout> {
    self.shm_client.as_ref().map(|c| c.layout())
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
  if !aec_idle && aec_state != AecState::Disabled { aec.latency_samples() } else { 0 }
}

#[cfg(test)]
mod tests;
