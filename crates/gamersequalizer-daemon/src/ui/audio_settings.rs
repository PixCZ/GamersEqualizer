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
  /// `mic_device_id` is the user's own pick. Files from older builds lack it: there the id may be
  /// a guess (the first mic in the list), so it is dropped once and the default mic is used.
  pub mic_device_picked: bool,
  pub mic_leveler_enabled: bool,
  pub mic_leveler_target_db_x10: i32,
  /// Windows mic volume the automatic input gain set, and the volume the user/app wants
  /// (`mic_volume_permille`, may be > 1000) at that time; 0 = no automatic level.
  pub mic_auto_gain_permille: u32,
  pub mic_auto_wish_permille: u32,
  pub sidetone_low_latency: bool,
  /// Automatic ChatMix, and how far the voice should stand out from the game (1000 = about 4x).
  pub duck_enabled: bool,
  pub duck_strength_permille: u32,
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
        crate::output_eq::settings::keep_corrupt(&path);
        Self::default()
      }),
      Err(_) => Self::default(),
    }
  }

  /// Keys a newer version wrote stay in the file: this struct is rebuilt from shared memory, so
  /// it cannot carry them itself.
  pub fn save(&self, dir: &Path) {
    let path = dir.join(FILE);
    let result = serde_json::to_value(self)
      .and_then(|mut now| {
        let old = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
        if let (Some(now), Some(serde_json::Value::Object(old))) = (now.as_object_mut(), old) {
          for (key, value) in old {
            now.entry(key).or_insert(value);
          }
        }
        serde_json::to_string_pretty(&now)
      })
      .map_err(std::io::Error::other)
      .and_then(|json| write_atomic(&path, &json));
    if let Err(e) = result {
      log::warn!("[AudioSettings] could not save {}: {e}", path.display());
    }
  }

  /// `self` is what was saved last: saves `now` and keeps it when it differs.
  pub fn save_if_changed(&mut self, now: Self, dir: &Path) {
    if now != *self {
      now.save(dir);
      *self = now;
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
      // Shared memory holds only picks (an empty id = follow the Windows default).
      mic_device_picked: h.get_selected_mic_id().is_some(),
      mic_leveler_enabled: h.mic_leveler_enabled.load(r),
      mic_leveler_target_db_x10: h.mic_leveler_target_db_x10.load(r),
      mic_auto_gain_permille: h.mic_auto_gain_permille.load(r),
      mic_auto_wish_permille: if h.mic_auto_gain_permille.load(r) > 0 { h.mic_volume_permille.load(r) } else { 0 },
      sidetone_low_latency: h.sidetone_low_latency.load(r),
      duck_enabled: h.duck_enabled.load(r),
      duck_strength_permille: h.duck_strength_permille.load(r),
    }
  }

  /// Fill a freshly created mapping (after its defaults and the Windows volumes are in).
  pub fn apply_to_header(&self, h: &mut ShmHeader) {
    let r = Ordering::Relaxed;
    h.sidetone_low_latency.store(self.sidetone_low_latency, r);
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
    h.duck_enabled.store(self.duck_enabled, r);
    h.duck_strength_permille.store(self.duck_strength_permille.min(1000), r);
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
    if let Some(id) = self.mic_device_id.as_ref().filter(|_| self.mic_device_picked) {
      h.set_selected_mic_id(id);
    }
  }
}

/// Take the mixer from shared memory (saved or current values) into a fresh controller, whose
/// channels otherwise start at 100 %. Channels on their own endpoint keep the Windows volume.
/// `service_runs`: a window starting next to the running service (`daemon_active`), which owns
/// the mic in use: its mic wish (and automatic level) are taken as they are. Not at the service's
/// own start, where the mapping may hold only the default capture device's volume.
pub fn restore_mixer(h: &ShmHeader, chatmix: &mut ChatMixController, service_runs: bool) {
  let r = Ordering::Relaxed;
  let level = |a: &std::sync::atomic::AtomicU32| a.load(r) as f32 / 1000.0;
  if chatmix.game_device_id.is_none() {
    chatmix.game.volume = level(&h.game_volume_permille);
    chatmix.game.muted = h.game_muted.load(r);
  }
  if chatmix.chat_device_id.is_none() {
    chatmix.chat.volume = level(&h.chat_volume_permille);
    chatmix.chat.muted = h.chat_muted.load(r);
  }
  if chatmix.media_device_id.is_none() {
    chatmix.media.volume = level(&h.media_volume_permille);
    chatmix.media.muted = h.media_muted.load(r);
  }
  chatmix.balance = h.chatmix_balance_permille.load(r) as f32 / 1000.0;
  let auto = h.mic_auto_gain_permille.load(r);
  let mic = level(&h.mic_volume_permille);
  if service_runs {
    // The window read its own guess of the mic (maybe the default capture device, not the mic
    // in use): writing that back moved the headset from 53 % to 40 % at window start.
    chatmix.mic.volume = mic;
    chatmix.mic.applied_volume = mic.min(1.0);
    chatmix.mic_hw_auto = (auto > 0).then_some(auto as f32 / 1000.0);
    return;
  }
  if auto > 0 {
    chatmix.adopt_mic_auto(mic, auto as f32 / 1000.0);
  }
  if mic > 1.0 && chatmix.mic.volume >= MIC_FULL {
    chatmix.mic.volume = mic;
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use gamersequalizer_common::shm::DenoiseModel;

  #[test]
  fn a_key_from_a_newer_version_survives_a_save() {
    let dir = std::env::temp_dir().join(format!("ge_audio_settings_newer_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(FILE), r#"{"aec_enabled": false, "voice_changer": {"pitch": 2}}"#).unwrap();
    let mut h = ShmHeader::default();
    AudioSettings::load_or_default(&dir).apply_to_header(&mut h);
    AudioSettings::from_header(&h).save(&dir);
    let text = std::fs::read_to_string(dir.join(FILE)).unwrap();
    assert!(text.contains(r#""voice_changer""#), "{text}");
    assert!(!AudioSettings::load_or_default(&dir).aec_enabled);
    let _ = std::fs::remove_dir_all(&dir);
  }

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
    h.sidetone_low_latency.store(true, Ordering::Relaxed);
    h.duck_enabled.store(true, Ordering::Relaxed);
    h.duck_strength_permille.store(800, Ordering::Relaxed);
    let mut h = h;
    h.set_selected_mic_id("{mic-1}");
    AudioSettings::from_header(&h).save(&dir);

    // Restart: a new mapping starts from defaults, Windows mic at 100 %.
    let mut fresh = ShmHeader::default();
    AudioSettings::load_or_default(&dir).apply_to_header(&mut fresh);
    assert!(fresh.sidetone_low_latency.load(Ordering::Relaxed));
    assert!(fresh.duck_enabled.load(Ordering::Relaxed));
    assert_eq!(fresh.duck_strength_permille.load(Ordering::Relaxed), 800);
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

    // An older build saved a guessed mic without the flag: it is not taken as a pick.
    std::fs::write(dir.join(FILE), r#"{ "mic_device_id": "{laptop-mic}" }"#).unwrap();
    let mut old = ShmHeader::default();
    AudioSettings::load_or_default(&dir).apply_to_header(&mut old);
    assert_eq!(old.get_selected_mic_id(), None);
    // Nor did it know automatic ChatMix: it gets the default, on, at the default strength.
    assert!(old.duck_enabled.load(Ordering::Relaxed));
    assert_eq!(old.duck_strength_permille.load(Ordering::Relaxed), 500);

    std::fs::write(dir.join(FILE), "{ nope").unwrap();
    assert_eq!(AudioSettings::load_or_default(&dir), AudioSettings::default());
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// Regression (log 2026-11-08 20:48:30): the window took the default capture device's 40 %
  /// and wrote it over the service's 53 % on the mic in use.
  #[test]
  fn window_next_to_the_service_keeps_its_mic_wish() {
    let h = ShmHeader::default();
    h.mic_volume_permille.store(530, Ordering::Relaxed);
    let mut window = ChatMixController::new();
    (window.mic_device_id, window.mic.volume) = (Some("{headset}".into()), 0.40);
    restore_mixer(&h, &mut window, true);
    assert_eq!((window.mic.volume, window.mic.applied_volume, window.mic_hw_auto), (0.53, 0.53, None));
    // A re-bind with the same mic (first frame, replug) does not re-read and undo the wish.
    assert!(!window.use_mic(window.mic_device_id.clone()));
    assert_eq!((window.mic.volume, window.mic.applied_volume), (0.53, 0.53));

    // Service start: only a boost over a Windows mic at 100 % is taken from the mapping.
    let mut service = ChatMixController::new();
    service.mic.volume = 0.40;
    restore_mixer(&h, &mut service, false);
    assert_eq!(service.mic.volume, 0.40);
    h.mic_volume_permille.store(2000, Ordering::Relaxed);
    service.mic.volume = 1.0;
    restore_mixer(&h, &mut service, false);
    assert_eq!(service.mic.volume, 2.0);
  }
}
