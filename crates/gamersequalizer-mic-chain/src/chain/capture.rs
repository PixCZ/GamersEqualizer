//! Debug capture (feature `debug-capture`): every stage and the per-block variables.

use super::*;

impl MicChain {
  /// Starts a capture block if one is recording: raw mic + the reference at mic time.
  #[cfg(feature = "debug-capture")]
  pub(super) fn capture_input(&mut self, mic_in: &[f32], mic_qpc_hns: Option<u64>) -> bool {
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
  pub(super) fn capture_finish(
    &mut self,
    frames: usize,
    mic_qpc_hns: Option<u64>,
    s: &MicSettings,
    boost: f32,
    load: f32,
  ) {
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
      self.ai_denoiser.selected_model().to_u32() as f32,
      self.ai_denoiser.active_model().to_u32() as f32,
      self.ai_denoiser.limit().to_u32() as f32,
      s.ai_strength_permille as f32 / 1000.0,
      s.gate_db as f32,
      b(self.ai_denoiser.heard_voice()),
      self.ai_denoiser.output_lead_samples() as f32,
      self.eq_version.map_or(-1.0, |v| v as f32),
      b(s.leveler_on),
      s.leveler_target_x10 as f32 / 10.0,
      self.leveler.gain_db(),
      self.leveler.level_db().unwrap_or(f32::NAN),
      s.input_gain_x10 as f32 / 10.0,
      s.mic_permille as f32 / 1000.0,
      boost,
      load,
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
