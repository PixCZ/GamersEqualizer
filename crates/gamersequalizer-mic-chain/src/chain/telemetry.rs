//! What the chain publishes for the app: levels, states, load and the per-window extremes.

use super::*;

/// A telemetry window is "overloaded" with this many late blocks (processing > block length)...
const OVERLOAD_LATE_BLOCKS: u32 = 2;
/// ...or this average load (time budget share; the audio engine needs the rest).
const OVERLOAD_AVG_LOAD: f32 = 0.9;
/// Consecutive overloaded windows (~100 ms each) before a heavy model gives way to RNNoise.
const OVERLOAD_WINDOWS_TO_FALL_BACK: u32 = 3;

/// Window of the published chain extremes (probability max, gate / limiter min): 250 ms at 48 kHz,
/// one daemon log tick.
const CHAIN_WINDOW_SAMPLES: usize = 12_000;

/// CPU time of the mic chain relative to real time, per telemetry window.
#[derive(Default)]
pub(super) struct DspLoad {
  peak: f32,
  sum: f32,
  blocks: u32,
  late_in_window: u32,
  /// Cumulative late blocks (published; the UI/log compares successive values).
  pub(super) late_total: u32,
  overloaded_windows: u32,
}

/// Chain state for the daemon's debug log: cumulative block counters, extremes per window.
#[derive(Default)]
pub(super) struct ChainWindow {
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

impl Default for ChainExtremes {
  fn default() -> Self {
    // Probabilities of -1 (no frame finished) never beat 0; NaN never wins `max`/`min`.
    Self { in_prob: 0.0, out_prob: 0.0, soft_prob: 0.0, gate_gain: 1.0, limiter_gain: 1.0 }
  }
}

impl ChainWindow {
  pub(super) fn add_block(&mut self, dn: &DenoiseDebug, heard_voice: bool, limiter_gain: f32, frames: usize) {
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
  pub(super) fn add(&mut self, share: f32) {
    self.peak = self.peak.max(share);
    self.sum += share;
    self.blocks += 1;
    if share > 1.0 {
      self.late_in_window += 1;
      self.late_total = self.late_total.wrapping_add(1);
    }
  }

  fn average(&self) -> f32 {
    if self.blocks == 0 { 0.0 } else { self.sum / self.blocks as f32 }
  }

  /// True when the heavy model should give way (only counted while one runs).
  pub(super) fn end_window(&mut self, deepfilter_active: bool) -> bool {
    let overloaded = self.late_in_window >= OVERLOAD_LATE_BLOCKS || self.average() > OVERLOAD_AVG_LOAD;
    self.overloaded_windows = if deepfilter_active && overloaded { self.overloaded_windows + 1 } else { 0 };
    if self.overloaded_windows >= OVERLOAD_WINDOWS_TO_FALL_BACK {
      self.overloaded_windows = 0;
      return true;
    }
    false
  }

  pub(super) fn reset_window(&mut self) {
    self.peak = 0.0;
    self.sum = 0.0;
    self.blocks = 0;
    self.late_in_window = 0;
  }
}

/// dBFS x 10, limited to -120..+20 dB (an inf/NaN level must not saturate to i32::MAX).
pub(super) fn db_x10(rms: f32) -> i32 {
  let rms = if rms.is_finite() { rms.clamp(1e-6, 10.0) } else { 1e-6 };
  (200.0 * rms.log10()).round() as i32
}

/// Echo strength for the mapping: 0 = none yet, a measured one negative (-0.1 dB at most), a
/// bound positive.
pub(super) fn echo_db_x10(strength: Option<(f32, bool)>) -> i32 {
  strength.map_or(0, |(db, bound)| {
    let x10 = ((db * 10.0).round() as i32).clamp(-1200, -1);
    if bound { -x10 } else { x10 }
  })
}

impl MicChain {
  pub(super) fn publish_telemetry(&self) {
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

    h.aec_echo_db_x10.store(echo_db_x10(self.aec.echo_strength_db()), Ordering::Relaxed);
    h.aec_fallback_echo_db_x10.store((self.aec.fallback_echo_db() * 10.0).round() as i32, Ordering::Relaxed);
    h.aec_mic_gain_db_x10.store((self.aec.mic_gain_db() * 10.0).round() as i32, Ordering::Relaxed);
    let (fit, slope) = self.aec.echo_fit();
    let signed_permille = |x: f32| (x * 1000.0).round().clamp(-10_000.0, 10_000.0) as i32;
    h.aec_echo_fit_permille.store(signed_permille(fit), Ordering::Relaxed);
    h.aec_echo_slope_permille.store(signed_permille(slope), Ordering::Relaxed);
    h.aec_ref_level_db_x10.store(db_x10(self.ref_rms), Ordering::Relaxed);
    h.aec_mic_level_db_x10.store(db_x10(self.mic_rms), Ordering::Relaxed);
    h.aec_out_level_db_x10.store(db_x10(self.out_rms), Ordering::Relaxed);
    h.aec_pre_delay_us.store(to_us(PRE_DELAY_SAMPLES as f32), Ordering::Relaxed);
    h.aec_corr_max_delay_us.store(to_us(DelayEstimator::max_delay_samples() as f32), Ordering::Relaxed);

    for (slot, v) in h.aec_corr.iter().zip(est.corr_curve()) {
      slot.store(v.to_bits(), Ordering::Relaxed);
    }
    // Last, with Release: a daemon that sees this seq also sees the delay written above.
    h.aec_reference_seq_seen.store(self.reference_seq_seen, Ordering::Release);
    h.aec_telemetry_seq.fetch_add(1, Ordering::Release);
  }
}
