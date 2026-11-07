//! Mic chain and mixer choices that otherwise live only in shared memory (lost when the window
//! and the service both exit): `%APPDATA%\GamersEqualizer\profiles\audio_settings.json`.
//! Written into a freshly created mapping before any DSP reads it; saved by the window when a
//! value changes. Missing or corrupt file -> defaults, logged; the app keeps running.

use crate::output_eq::settings::write_atomic;
use crate::win_audio::ChatMixController;
use gamersequalizer_common::shm::ShmHeader;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::Ordering;

const FILE: &str = "audio_settings.json";
/// Windows mic volume counts as "at 100 %" from here: the saved digital boost applies on top.
const MIC_FULL: f32 = 0.995;
/// Windows mic volumes this close (permille) are the same level (the slider rounds).
const MIC_SAME_PERMILLE: u32 = 15;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
  pub aec_enabled: bool,
  pub denoise_strength_permille: u32,
  pub noise_gate_db: i32,
  /// `DenoiseModel::to_u32`; a model from a newer version stays as its number.
  pub denoise_model: u32,
  pub game_volume_permille: u32,
  pub chat_volume_permille: u32,
  pub media_volume_permille: u32,
  pub game_muted: bool,
  pub chat_muted: bool,
  pub media_muted: bool,
  pub balance_permille: i32,
  /// Mic level above Windows' 100 % (digital boost), 1000 = no boost. Below 100 % Windows owns it.
  pub mic_boost_permille: u32,
  pub mic_device_id: Option<String>,
  pub mic_leveler_enabled: bool,
  pub mic_leveler_target_db_x10: i32,
  /// Windows mic volume the automatic input gain set, and the volume the user/app wants
  /// (`mic_volume_permille`, may be > 1000) at that time; 0 = no automatic level.
  pub mic_auto_gain_permille: u32,
  pub mic_auto_wish_permille: u32,
}

impl Default for AudioSettings {
  fn default() -> Self {
    Self::from_header(&ShmHeader::default())
  }
}

impl AudioSettings {
  pub fn load_or_default(dir: &Path) -> Self {
    let path = dir.join(FILE);
    match std::fs::read_to_string(&path) {
      Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
        log::warn!("[AudioSettings] {} is corrupt ({e}), using defaults", path.display());
        Self::default()
      }),
      Err(_) => Self::default(),
    }
  }

  pub fn save(&self, dir: &Path) {
    let path = dir.join(FILE);
    let result =
      serde_json::to_string_pretty(self).map_err(std::io::Error::other).and_then(|json| write_atomic(&path, &json));
    if let Err(e) = result {
      log::warn!("[AudioSettings] could not save {}: {e}", path.display());
    }
  }

  /// What the user has set right now (the window keeps shared memory current every frame).
  pub fn from_header(h: &ShmHeader) -> Self {
    let r = Ordering::Relaxed;
    Self {
      aec_enabled: h.aec_enabled.load(r),
      denoise_strength_permille: h.ai_voice_strength_permille.load(r),
      noise_gate_db: h.noise_gate_threshold_db.load(r),
      denoise_model: h.denoise_model.load(r),
      game_volume_permille: h.game_volume_permille.load(r),
      chat_volume_permille: h.chat_volume_permille.load(r),
      media_volume_permille: h.media_volume_permille.load(r),
      game_muted: h.game_muted.load(r),
      chat_muted: h.chat_muted.load(r),
      media_muted: h.media_muted.load(r),
      balance_permille: h.chatmix_balance_permille.load(r),
      mic_boost_permille: h.mic_volume_permille.load(r).max(1000),
      mic_device_id: h.get_selected_mic_id(),
      mic_leveler_enabled: h.mic_leveler_enabled.load(r),
      mic_leveler_target_db_x10: h.mic_leveler_target_db_x10.load(r),
      mic_auto_gain_permille: h.mic_auto_gain_permille.load(r),
      mic_auto_wish_permille: if h.mic_auto_gain_permille.load(r) > 0 { h.mic_volume_permille.load(r) } else { 0 },
    }
  }

  /// Fill a freshly created mapping (after its defaults and the Windows volumes are in).
  pub fn apply_to_header(&self, h: &mut ShmHeader) {
    let r = Ordering::Relaxed;
    h.aec_enabled.store(self.aec_enabled, r);
    h.ai_voice_strength_permille.store(self.denoise_strength_permille.min(1000), r);
    h.noise_gate_threshold_db.store(self.noise_gate_db, r);
    h.denoise_model.store(self.denoise_model, r);
    h.game_volume_permille.store(self.game_volume_permille, r);
    h.chat_volume_permille.store(self.chat_volume_permille, r);
    h.media_volume_permille.store(self.media_volume_permille, r);
    h.game_muted.store(self.game_muted, r);
    h.chat_muted.store(self.chat_muted, r);
    h.media_muted.store(self.media_muted, r);
    h.chatmix_balance_permille.store(self.balance_permille.clamp(-1000, 1000), r);
    h.mic_leveler_enabled.store(self.mic_leveler_enabled, r);
    h.mic_leveler_target_db_x10.store(self.mic_leveler_target_db_x10.clamp(-360, -120), r);
    // The boost only makes sense while Windows is still at 100 % (someone may have lowered it).
    if self.mic_boost_permille > 1000 && h.mic_volume_permille.load(r) as f32 / 1000.0 >= MIC_FULL {
      h.mic_volume_permille.store(self.mic_boost_permille, r);
    }
    // Windows still shows the automatic level: the wish behind it comes back too.
    let auto = self.mic_auto_gain_permille;
    if auto > 0 && h.mic_volume_permille.load(r).abs_diff(auto) <= MIC_SAME_PERMILLE {
      h.mic_volume_permille.store(self.mic_auto_wish_permille, r);
      h.mic_auto_gain_permille.store(auto, r);
    }
    if let Some(id) = &self.mic_device_id {
      h.set_selected_mic_id(id);
    }
  }
}

/// Take the mixer from shared memory (saved or current values) into a fresh controller, whose
/// channels otherwise start at 100 %. Channels on their own endpoint keep the Windows volume.
pub fn restore_mixer(h: &ShmHeader, chatmix: &mut ChatMixController) {
  let r = Ordering::Relaxed;
  let level = |a: &std::sync::atomic::AtomicU32| a.load(r) as f32 / 1000.0;
  if chatmix.game_device_id.is_none() {
    chatmix.game_volume = level(&h.game_volume_permille);
    chatmix.game_muted = h.game_muted.load(r);
  }
  if chatmix.chat_device_id.is_none() {
    chatmix.chat_volume = level(&h.chat_volume_permille);
    chatmix.chat_muted = h.chat_muted.load(r);
  }
  if chatmix.media_device_id.is_none() {
    chatmix.media_volume = level(&h.media_volume_permille);
    chatmix.media_muted = h.media_muted.load(r);
  }
  chatmix.balance = h.chatmix_balance_permille.load(r) as f32 / 1000.0;
  let auto = h.mic_auto_gain_permille.load(r);
  if auto > 0 {
    chatmix.adopt_mic_auto(level(&h.mic_volume_permille), auto as f32 / 1000.0);
  }
  let mic = level(&h.mic_volume_permille);
  if mic > 1.0 && chatmix.mic_volume >= MIC_FULL {
    chatmix.mic_volume = mic;
  }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
  use super::*;
  use gamersequalizer_common::shm::DenoiseModel;

  #[test]
  fn saved_choices_survive_a_new_mapping() {
    let dir = std::env::temp_dir().join(format!("ge_audio_settings_test_{}", std::process::id()));
    assert_eq!(AudioSettings::load_or_default(&dir), AudioSettings::default());

    let h = ShmHeader::default();
    h.denoise_model.store(DenoiseModel::DpdfNet.to_u32(), Ordering::Relaxed);
    h.aec_enabled.store(false, Ordering::Relaxed);
    h.mic_leveler_enabled.store(false, Ordering::Relaxed);
    h.mic_leveler_target_db_x10.store(-180, Ordering::Relaxed);
    h.game_volume_permille.store(400, Ordering::Relaxed);
    h.mic_volume_permille.store(2000, Ordering::Relaxed);
    let mut h = h;
    h.set_selected_mic_id("{mic-1}");
    AudioSettings::from_header(&h).save(&dir);

    // Restart: a new mapping starts from defaults, Windows mic at 100 %.
    let mut fresh = ShmHeader::default();
    AudioSettings::load_or_default(&dir).apply_to_header(&mut fresh);
    assert_eq!(DenoiseModel::from_u32(fresh.denoise_model.load(Ordering::Relaxed)), DenoiseModel::DpdfNet);
    assert!(!fresh.aec_enabled.load(Ordering::Relaxed));
    assert!(!fresh.mic_leveler_enabled.load(Ordering::Relaxed));
    assert_eq!(fresh.mic_leveler_target_db_x10.load(Ordering::Relaxed), -180);
    assert_eq!(fresh.game_volume_permille.load(Ordering::Relaxed), 400);
    assert_eq!(fresh.mic_volume_permille.load(Ordering::Relaxed), 2000);
    assert_eq!(fresh.get_selected_mic_id().as_deref(), Some("{mic-1}"));

    // Windows mic lowered meanwhile: the boost is not forced back on.
    let mut lowered = ShmHeader::default();
    lowered.mic_volume_permille.store(500, Ordering::Relaxed);
    AudioSettings::load_or_default(&dir).apply_to_header(&mut lowered);
    assert_eq!(lowered.mic_volume_permille.load(Ordering::Relaxed), 500);

    std::fs::write(dir.join(FILE), "{ nope").unwrap();
    assert_eq!(AudioSettings::load_or_default(&dir), AudioSettings::default());
    let _ = std::fs::remove_dir_all(&dir);
  }
}
