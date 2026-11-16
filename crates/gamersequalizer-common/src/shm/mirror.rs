//! Mirroring between the daemon's mapping and the APO's: settings one way, telemetry back.

use super::*;

/// Source EQ versions a mirror (`ShmHeader::copy_controls_from`) copied last. Lives in the
/// copying process, not in shared memory; `default()` = nothing copied yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopiedEqVersions {
  mic: Option<u32>,
  sidetone: Option<u32>,
  out: Option<u32>,
  streams: [Option<u32>; STREAM_EQ_BUSES],
}

impl ShmHeader {
  /// GUI -> DSP settings, for mirroring the daemon's mapping into the APO's. An EQ is copied
  /// when its source version differs from the one `seen` remembers (a new owner starts from
  /// `CopiedEqVersions::default()`: copies everything). The destination keeps its own counters
  /// (each write bumps them), so the APO always sees a change: storing the source's number
  /// could land on exactly the version the APO had cached from the previous owner.
  pub fn copy_controls_from(&self, src: &ShmHeader, seen: &mut CopiedEqVersions) {
    let r = Ordering::Relaxed;
    self.aec_enabled.store(src.aec_enabled.load(r), r);
    self.ai_voice_strength_permille.store(src.ai_voice_strength_permille.load(r), r);
    self.noise_gate_threshold_db.store(src.noise_gate_threshold_db.load(r), r);
    self.mic_volume_permille.store(src.mic_volume_permille.load(r), r);
    self.mic_muted.store(src.mic_muted.load(r), r);
    self.denoise_model.store(src.denoise_model.load(r), r);
    self.denoise_retry_seq.store(src.denoise_retry_seq.load(r), r);
    self.master_bypass.store(src.master_bypass.load(r), r);
    self.mic_leveler_enabled.store(src.mic_leveler_enabled.load(r), r);
    self.mic_leveler_target_db_x10.store(src.mic_leveler_target_db_x10.load(r), r);
    self.mic_leveler_reset_seq.store(src.mic_leveler_reset_seq.load(r), r);
    self.mic_input_gain_db_x10.store(src.mic_input_gain_db_x10.load(r), r);
    // Sidetone: the EQ and the values first, `enabled` last (as `write_sidetone` does), so
    // the APOs never run a fresh "on" with the previous pair's values.
    let version = src.sidetone_eq_version.load(Ordering::Acquire);
    if seen.sidetone != Some(version) {
      self.write_sidetone_eq(&src.read_sidetone_eq().collect::<Vec<_>>());
      seen.sidetone = Some(version);
    }
    self.sidetone_noise_mode.store(src.sidetone_noise_mode.load(r), r);
    self.sidetone_volume_permille.store(src.sidetone_volume_permille.load(r), r);
    self.sidetone_gate_margin_db_x10.store(src.sidetone_gate_margin_db_x10.load(r), r);
    self.sidetone_gate_range_db_x10.store(src.sidetone_gate_range_db_x10.load(r), r);
    self.sidetone_mic_hash.store(src.sidetone_mic_hash.load(r), r);
    self.sidetone_output_hash.store(src.sidetone_output_hash.load(r), r);
    self.sidetone_enabled.store(src.sidetone_enabled.load(r), Ordering::Release);
    // Version first, bands after: a write in between is copied now and again next time.
    let version = src.mic_eq_version.load(Ordering::Acquire);
    if seen.mic != Some(version) {
      self.write_mic_eq(&src.read_mic_eq().collect::<Vec<_>>());
      seen.mic = Some(version);
    }
    let version = src.out_eq_version.load(Ordering::Acquire);
    if seen.out != Some(version) {
      let (bands, preamp) = src.read_out_eq();
      self.write_out_eq(&bands.collect::<Vec<_>>(), preamp);
      seen.out = Some(version);
    }
    for ((dst, src), seen) in self.stream_eq.iter().zip(&src.stream_eq).zip(&mut seen.streams) {
      let version = src.version.load(Ordering::Acquire);
      if *seen != Some(version) {
        let (bands, preamp) = src.read();
        dst.write(&bands.collect::<Vec<_>>(), preamp);
        *seen = Some(version);
      }
    }
  }

  /// Output APO -> GUI telemetry (independent of the mic APO running).
  pub fn copy_output_telemetry_from(&self, src: &ShmHeader) {
    let r = Ordering::Relaxed;
    self.out_apo_blocks.store(src.out_apo_blocks.load(r), r);
    self.out_apo_sample_rate.store(src.out_apo_sample_rate.load(r), r);
    self.out_apo_channels.store(src.out_apo_channels.load(r), r);
    self.sidetone_reader_beat.store(src.sidetone_reader_beat.load(r), r);
    self.sidetone_out_rate.store(src.sidetone_out_rate.load(r), r);
    self.sidetone_out_block_frames.store(src.sidetone_out_block_frames.load(r), r);
    self.sidetone_queued_us.store(src.sidetone_queued_us.load(r), r);
    self.sidetone_age_us.store(src.sidetone_age_us.load(r), r);
    self.sidetone_underruns.store(src.sidetone_underruns.load(r), r);
    self.sidetone_resyncs.store(src.sidetone_resyncs.load(r), r);
    self.sidetone_output_hash_seen.store(src.sidetone_output_hash_seen.load(r), r);
    // The mic APO's sidetone tap too: it runs without the mic chain (whose telemetry
    // `copy_dsp_telemetry_from` waits for), e.g. after the chain failed.
    self.sidetone_writer_beat.store(src.sidetone_writer_beat.load(r), r);
    self.sidetone_mic_rate.store(src.sidetone_mic_rate.load(r), r);
    self.sidetone_mic_block_frames.store(src.sidetone_mic_block_frames.load(r), r);
    self.sidetone_peak_permille.store(src.sidetone_peak_permille.load(r), r);
    self.sidetone_gate_gain_db_x10.store(src.sidetone_gate_gain_db_x10.load(r), r);
    self.sidetone_noise_mode_active.store(src.sidetone_noise_mode_active.load(r), r);
    self.sidetone_noise_delay_us.store(src.sidetone_noise_delay_us.load(r), r);
    self.sidetone_mic_hash_seen.store(src.sidetone_mic_hash_seen.load(r), r);
  }

  /// DSP -> GUI telemetry, for mirroring the APO's mapping into the daemon's.
  pub fn copy_dsp_telemetry_from(&self, src: &ShmHeader) {
    let r = Ordering::Relaxed;
    self.erle_db_times_10.store(src.erle_db_times_10.load(r), r);
    self.denoise_model_active.store(src.denoise_model_active.load(r), r);
    self.denoise_limit.store(src.denoise_limit.load(r), r);
    self.mic_dsp_load_peak_permille.store(src.mic_dsp_load_peak_permille.load(r), r);
    self.mic_dsp_load_avg_permille.store(src.mic_dsp_load_avg_permille.load(r), r);
    self.mic_dsp_overruns.store(src.mic_dsp_overruns.load(r), r);
    self.mic_leveler_gain_db_x10.store(src.mic_leveler_gain_db_x10.load(r), r);
    self.mic_leveler_level_db_x10.store(src.mic_leveler_level_db_x10.load(r), r);
    self.mic_input_clips.store(src.mic_input_clips.load(r), r);
    self.mic_speech_ms.store(src.mic_speech_ms.load(r), r);
    self.mic_call_jitter_peak_us.store(src.mic_call_jitter_peak_us.load(r), r);
    self.mic_call_gaps.store(src.mic_call_gaps.load(r), r);
    self.mic_latency_measured_us.store(src.mic_latency_measured_us.load(r), r);
    self.mic_latency_aec_us.store(src.mic_latency_aec_us.load(r), r);
    self.mic_latency_denoise_us.store(src.mic_latency_denoise_us.load(r), r);
    self.mic_latency_lookahead_us.store(src.mic_latency_lookahead_us.load(r), r);
    self.mic_voice_blocks.store(src.mic_voice_blocks.load(r), r);
    self.mic_blocks.store(src.mic_blocks.load(r), r);
    self.mic_in_prob_max_permille.store(src.mic_in_prob_max_permille.load(r), r);
    self.mic_out_prob_max_permille.store(src.mic_out_prob_max_permille.load(r), r);
    self.mic_soft_prob_max_permille.store(src.mic_soft_prob_max_permille.load(r), r);
    self.mic_gate_gain_min_db_x10.store(src.mic_gate_gain_min_db_x10.load(r), r);
    self.mic_gate_floor_db_x10.store(src.mic_gate_floor_db_x10.load(r), r);
    self.mic_df_pre_gain_db_x10.store(src.mic_df_pre_gain_db_x10.load(r), r);
    self.mic_rescue_blocks.store(src.mic_rescue_blocks.load(r), r);
    self.mic_chain_flags.store(src.mic_chain_flags.load(r), r);
    self.mic_quiet_ms.store(src.mic_quiet_ms.load(r), r);
    self.mic_limiter_gain_min_db_x10.store(src.mic_limiter_gain_min_db_x10.load(r), r);
    self.aec_echo_db_x10.store(src.aec_echo_db_x10.load(r), r);
    self.aec_fallback_echo_db_x10.store(src.aec_fallback_echo_db_x10.load(r), r);
    self.aec_mic_gain_db_x10.store(src.aec_mic_gain_db_x10.load(r), r);
    self.aec_echo_fit_permille.store(src.aec_echo_fit_permille.load(r), r);
    self.aec_echo_slope_permille.store(src.aec_echo_slope_permille.load(r), r);
    self.aec_state.store(src.aec_state.load(r), r);
    self.aec_delay_us.store(src.aec_delay_us.load(r), r);
    self.aec_raw_delay_us.store(src.aec_raw_delay_us.load(r), r);
    self.aec_confidence_permille.store(src.aec_confidence_permille.load(r), r);
    self.aec_estimate_count.store(src.aec_estimate_count.load(r), r);
    self.aec_delay_from_cache.store(src.aec_delay_from_cache.load(r), r);
    self.aec_ref_level_db_x10.store(src.aec_ref_level_db_x10.load(r), r);
    self.aec_mic_level_db_x10.store(src.aec_mic_level_db_x10.load(r), r);
    self.aec_out_level_db_x10.store(src.aec_out_level_db_x10.load(r), r);
    self.aec_pre_delay_us.store(src.aec_pre_delay_us.load(r), r);
    self.aec_corr_max_delay_us.store(src.aec_corr_max_delay_us.load(r), r);
    for (d, s) in self.aec_corr.iter().zip(&src.aec_corr) {
      d.store(s.load(r), r);
    }
    self.aec_telemetry_seq.fetch_add(1, Ordering::Release);
  }
}
