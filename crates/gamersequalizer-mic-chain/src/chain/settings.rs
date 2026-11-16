//! The chain's settings, read from shared memory once per block.

use super::*;

/// The chain's settings, read from shared memory once per block (defaults without a mapping).
/// Raw values: the leveler compares them as they are (`LevelerInputs`), the capture records them.
pub(super) struct MicSettings {
  pub(super) aec_enabled: bool,
  pub(super) ai_strength_permille: u32,
  /// `NOISE_GATE_*` or a threshold, dB.
  pub(super) gate_db: i32,
  /// Mic volume, 1000 = 100 % (0 in the mapping reads as 100 %).
  pub(super) mic_permille: u32,
  pub(super) model: DenoiseModel,
  pub(super) denoise_retry_seq: u32,
  pub(super) eq_version: Option<u32>,
  pub(super) cached_delay: u32,
  pub(super) cached_echo_x10: i32,
  /// None without a mapping: the output counts as unchanged.
  pub(super) reference_seq: Option<u32>,
  pub(super) leveler_on: bool,
  pub(super) input_gain_x10: i32,
  pub(super) leveler_target_x10: i32,
  pub(super) leveler_reset_seq: u32,
  pub(super) leveler_saved_x10: i32,
}

impl MicSettings {
  pub(super) fn read(header: Option<&ShmHeader>) -> Self {
    let Some(h) = header else {
      return Self {
        aec_enabled: true,
        ai_strength_permille: 750,
        gate_db: NOISE_GATE_AUTO_LOOKAHEAD,
        mic_permille: 1000,
        model: DenoiseModel::RnNoise,
        denoise_retry_seq: 0,
        eq_version: None,
        cached_delay: 0,
        cached_echo_x10: 0,
        reference_seq: None,
        leveler_on: true,
        input_gain_x10: 0,
        leveler_target_x10: MIC_LEVELER_DEFAULT_TARGET_DB_X10,
        leveler_reset_seq: 0,
        leveler_saved_x10: 0,
      };
    };
    let relaxed = Ordering::Relaxed;
    // Acquire before the cached delay: the daemon stores the new output's delay first.
    let reference_seq = Some(h.aec_reference_seq.load(Ordering::Acquire));
    Self {
      aec_enabled: h.aec_enabled.load(relaxed),
      ai_strength_permille: h.ai_voice_strength_permille.load(relaxed),
      gate_db: h.noise_gate_threshold_db.load(relaxed),
      mic_permille: Some(h.mic_volume_permille.load(relaxed)).filter(|&p| p > 0).unwrap_or(1000),
      model: DenoiseModel::from_u32(h.denoise_model.load(relaxed)),
      denoise_retry_seq: h.denoise_retry_seq.load(relaxed),
      eq_version: Some(h.mic_eq_version.load(Ordering::Acquire)),
      cached_delay: h.aec_cached_delay_samples.load(relaxed),
      cached_echo_x10: h.aec_cached_echo_db_x10.load(relaxed),
      reference_seq,
      leveler_on: h.mic_leveler_enabled.load(relaxed),
      input_gain_x10: h.mic_input_gain_db_x10.load(relaxed),
      leveler_target_x10: h.mic_leveler_target_db_x10.load(relaxed),
      leveler_reset_seq: h.mic_leveler_reset_seq.load(relaxed),
      leveler_saved_x10: h.mic_leveler_saved_level_db_x10.load(relaxed),
    }
  }
}
