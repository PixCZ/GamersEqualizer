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
/// Reference older than 200 ms means nothing is playing (loopback delivers no packets).
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
      self.leveler.process(mic, self.ai_denoiser.heard_level_voice(), self.ai_denoiser.output_lead_samples());
    } else {
      self.leveler.reset();
    }
    #[cfg(feature = "debug-capture")]
    if let Some(cap) = self.capture.as_ref().filter(|_| capturing) {
      cap.stage(STAGE_AFTER_LEVELER, mic);
    }

    // Stage 5: volume + peak limiter, always last so EQ boosts can't clip. With the leveler the
    // mic volume (0..100 %) is how loud others hear you: 100 % = the target, 50 % = 6 dB under it
    // (the leveler took the Windows volume out of the input); a saved boost above 100 % is cut, the
    // target does that job. Without it: only the boost > 100 % (up to 300 %).
    let boost = if s.leveler_on {
      s.mic_permille.min(1000) as f32 / 1000.0
    } else if s.mic_permille > 1000 {
      s.mic_permille as f32 / 1000.0
    } else {
      1.0
    };
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
mod tests {
  use super::*;
  use gamersequalizer_common::shm::StereoFrame;

  /// The whole mic chain (no AEC reference, default mic EQ) on the private recordings, for
  /// before/after comparisons of the stages after the denoiser. Per model and input level:
  /// output level of voice / whisper / room sections (dBFS, power mean), samples at full scale
  /// (>= 0.99, per mille), peak, leveler gain at the end; and keys.wav alone. Run:
  /// `cargo test -p gamersequalizer-mic-chain --release chain_report -- --ignored --nocapture`
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
      let mut p = MicChain::with_shm(None);
      p.test_overrides = Some((model, permille));
      p.ai_denoiser.set_model(model);
      p.eq.set_bands(&bands, 48_000.0);
      let mut out = vec![0.0f32; input.len()];
      let mut asleep = 0usize;
      let whisper_blocks = 40 * HALF_SEC / 480..48 * HALF_SEC / 480;
      for (n, (i, o)) in input.chunks(480).zip(out.chunks_mut(480)).enumerate() {
        p.process_timed(&[i], &mut [o], None);
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
  fn test_echo_strength_sign_says_measured_or_bound() {
    assert_eq!(echo_db_x10(None), 0);
    assert_eq!(echo_db_x10(Some((-25.04, false))), -250);
    assert_eq!(echo_db_x10(Some((-25.04, true))), 250);
    assert_eq!(echo_db_x10(Some((-0.01, false))), -1, "a measured 0 dB is not \"none\"");
    assert_eq!(echo_db_x10(Some((-0.01, true))), 1);
    assert_eq!(echo_db_x10(Some((f32::NEG_INFINITY, true))), 1200);
  }

  /// Another speaker output (`aec_reference_seq`): the delay from the old one is dropped and the
  /// new output's remembered delay is taken, which the daemon stored before the switch.
  #[test]
  fn test_other_speakers_start_from_their_remembered_delay() {
    let mut p = chain_with_reference("reference_seq", &noise(120_000, 5));
    let h = p.shm_header().expect("mapping");
    h.aec_cached_delay_samples.store(960, Ordering::Relaxed);
    let (mic, mut out) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    p.process_timed(&[&mic], &mut [&mut out], Some(block_hns(0)));
    assert_eq!(p.delay_estimator.current_delay(), Some(960));
    let h = p.shm_header().expect("mapping");
    h.aec_cached_delay_samples.store(1_200, Ordering::Relaxed);
    p.process_timed(&[&mic], &mut [&mut out], Some(block_hns(1)));
    assert_eq!(p.delay_estimator.current_delay(), Some(960), "the same output keeps its delay");
    p.shm_header().expect("mapping").aec_reference_seq.fetch_add(1, Ordering::Release);
    for k in 2..12 {
      p.process_timed(&[&mic], &mut [&mut out], Some(block_hns(k)));
    }
    assert_eq!(p.delay_estimator.current_delay(), Some(1_200));
    assert!(p.delay_estimator.is_from_cache());
    let seen = p.shm_header().expect("mapping").aec_reference_seq_seen.load(Ordering::Acquire);
    assert_eq!(seen, 1, "published for the daemon");
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
    let mut p = chain_with_reference("nan_delay", &reference);
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
    let mut p = chain_with_reference("aec_idle", &noise(120_000, 2));
    p.shm_header().expect("mapping").aec_cached_delay_samples.store(960, Ordering::Relaxed); // AEC runs at once
    let (loud, mut out) = (noise(BLOCK, 3), vec![0.0f32; BLOCK]);
    for k in 0..20 {
      p.process_timed(&[&loud], &mut [&mut out], Some(block_hns(k)));
    }
    assert!(!p.aec_idle, "the AEC ran ({:?})", p.aec_state);
    p.shm_header().expect("mapping").aec_enabled.store(false, Ordering::Relaxed);
    p.process_timed(&[&loud], &mut [&mut out], Some(block_hns(20)));
    p.shm_header().expect("mapping").aec_enabled.store(true, Ordering::Relaxed);
    // The AEC stage alone (later stages keep their own history): silence in, silence out. Two
    // whole frames: the first frame AEC3 puts out after the gap starts with its own delay line.
    for k in 0..2 {
      p.mic_mono_temp[..FRAME_480].fill(0.0);
      p.run_aec(FRAME_480, Some(block_hns(21) + k * 100_000));
      let peak = p.mic_mono_temp[..FRAME_480].iter().fold(0.0f32, |m, s| m.max(s.abs()));
      assert!(peak < 1e-3, "block {k}: samples from before the switch came out (peak {peak})");
    }
  }

  /// Safety net for changes that must not change the sound: speech, keys and an echo of the
  /// reference through the whole chain (AEC, RNNoise, leveler, limiter), with a pause, a master
  /// bypass, AEC off and on and "Learn again" on the way. Energy per second of the output must
  /// stay what it was; a change that is meant to alter the sound updates `EXPECTED` with a reason.
  #[test]
  fn test_whole_chain_output_stays_the_same() {
    use crate::ai_denoiser::tests::read_wav_48k;
    const SECONDS: usize = 8;
    const ECHO_DELAY: usize = 960;
    // G1 (2026-11-16): seconds 2.. changed, AEC3's first frame after a pause is silenced.
    const EXPECTED: [f64; SECONDS] = [5.0476, 568.84, 113.22, 74.089, 196.25, 172.34, 67.444, 68.037];
    let speech = read_wav_48k("speech_clean.wav");
    let keys = read_wav_48k("1-137-A-32.wav");
    let reference: Vec<f32> = noise(SECONDS * 48_000 + 48_000, 7).iter().map(|s| s * 0.4).collect();
    let mut p = chain_with_reference("golden", &reference);
    let header = p.shm_header().expect("mapping");
    header.aec_cached_delay_samples.store(ECHO_DELAY as u32, Ordering::Relaxed);
    let mut energy = [0.0f64; SECONDS];
    let mut out = vec![0.0f32; FRAME_480];
    for k in 0..SECONDS * 100 {
      let t = T0_HNS + 5_000_000 + (k * FRAME_480) as u64 * 10_000_000 / 48_000;
      let start = ref_position(&p, t, reference.len());
      let mic: Vec<f32> = (0..FRAME_480)
        .map(|i| {
          let n = k * FRAME_480 + i;
          let echo = (start + i).checked_sub(ECHO_DELAY).map_or(0.0, |j| reference[j]);
          speech[n % speech.len()] + 0.5 * keys[n % keys.len()] + 0.2 * echo
        })
        .collect();
      let h = p.shm_header().expect("mapping");
      match k {
        200 => p.pause(),
        300..305 => h.master_bypass.store(true, Ordering::Relaxed),
        400..410 => h.aec_enabled.store(false, Ordering::Relaxed),
        500 => _ = h.mic_leveler_reset_seq.fetch_add(1, Ordering::Relaxed),
        _ => {
          h.master_bypass.store(false, Ordering::Relaxed);
          h.aec_enabled.store(true, Ordering::Relaxed);
        }
      }
      p.process_timed(&[&mic], &mut [&mut out], Some(t));
      energy[k / 100] += out.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>();
    }
    eprintln!("energy per second: {energy:?}");
    for (second, (got, want)) in energy.iter().zip(EXPECTED).enumerate() {
      assert!((got - want).abs() <= 1e-3 * want.abs().max(1e-6), "second {second}: {got} vs {want}");
    }
  }

  /// "Learn again" blocks the saved level until the daemon clears it; a level it saves after that
  /// is the mic's again and seeds the leveler (regression: blocked for the rest of the stream).
  #[test]
  fn test_saved_level_seeds_again_once_the_daemon_cleared_it() {
    let mut p = chain_with_reference("seed", &noise(4_800, 4));
    let silence = [0.0f32; FRAME_480];
    let mut out = [0.0f32; FRAME_480];
    let mut block = |p: &mut MicChain, saved_x10: i32| {
      p.shm_header().expect("mapping").mic_leveler_saved_level_db_x10.store(saved_x10, Ordering::Relaxed);
      p.process_timed(&[&silence], &mut [&mut out], None);
      p.leveler.level_db().map(|db| db.round())
    };
    assert_eq!(block(&mut p, -200), Some(-20.0), "seeded");
    p.shm_header().expect("mapping").mic_leveler_reset_seq.fetch_add(1, Ordering::Relaxed);
    assert_eq!(block(&mut p, -200), None, "the stale level is not taken back");
    p.shm_header().expect("mapping").mic_leveler_enabled.store(false, Ordering::Relaxed);
    assert_eq!(block(&mut p, 0), None, "cleared by the daemon");
    p.shm_header().expect("mapping").mic_leveler_enabled.store(true, Ordering::Relaxed);
    assert_eq!(block(&mut p, -150), Some(-15.0), "a level saved since seeds again");
  }

  const FRAME_480: usize = 480;

  /// Blocks that don't line up with AEC3's 480-sample frames, so it queues output.
  const BLOCK: usize = 441;
  const T0_HNS: u64 = 10_000_000_000;

  /// Chain on a private mapping whose loopback reference played `reference` from `T0_HNS`;
  /// RNNoise, 100 % volume.
  fn chain_with_reference(tag: &str, reference: &[f32]) -> MicChain {
    let name = format!("Local\\GamersEqualizer_test_{tag}_{}", std::process::id());
    let mut shm = SharedMemoryClient::create_named(&name, None).expect("test mapping");
    let frames: Vec<StereoFrame> = reference.iter().map(|&s| StereoFrame { left: s, right: s }).collect();
    shm.ring_buffer().push_frames_timed(&frames, T0_HNS);
    shm.layout().header.tap_active.store(true, Ordering::Relaxed); // the loopback writer's flag
    let mut p = MicChain::with_shm(Some(shm));
    p.test_overrides = Some((DenoiseModel::RnNoise, 1000));
    p
  }

  /// QPC time of mic block `k`, from 0.5 s into the reference.
  fn block_hns(k: usize) -> u64 {
    T0_HNS + 5_000_000 + (k * BLOCK) as u64 * 10_000_000 / 48_000
  }

  /// Position in the reference (`len` samples, all written) that played at `t`.
  fn ref_position(p: &MicChain, t: u64, len: usize) -> usize {
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
