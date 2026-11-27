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
/// Keys the service writes on its own (`read_key` / `write_key`), outside `AudioSettings`: the
/// struct is rebuilt from shared memory, which does not hold them, and a save keeps keys it lacks.
const VOLUME_APPS_KEY: &str = "mic_volume_apps";
const SENSITIVITY_KEY: &str = "mic_sensitivity_permille";
/// Recordings in which an app moved the mic volume back and forth before it is on the list.
pub const VOLUME_APP_LISTED: u32 = 2;

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
    if let Some(id) = self.mic_device_id.as_ref().filter(|_| self.mic_device_picked) {
      h.set_selected_mic_id(id);
    }
  }
}

/// Take the mixer from shared memory (saved or current values) into a fresh controller, whose
/// channels otherwise start at 100 %. Channels on their own endpoint keep the Windows volume.
/// `service_runs`: a window starting next to the running service (`daemon_active`), which owns
/// the mic in use: its mic volume is taken as it is. Not at the service's own start, where the
/// mapping may hold only the default capture device's volume.
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
  let mic = level(&h.mic_volume_permille);
  if service_runs {
    // The window read its own guess of the mic (maybe the default capture device, not the mic
    // in use): writing that back moved the headset from 53 % to 40 % at window start.
    chatmix.mic.volume = mic;
    chatmix.mic.applied_volume = mic.min(1.0);
    return;
  }
  if mic > 1.0 && chatmix.mic.volume >= MIC_FULL {
    chatmix.mic.volume = mic;
  }
}

/// An app that moved the Windows mic volume back and forth during a recording (its own automatic
/// gain control): from `VOLUME_APP_LISTED` recordings on, the voice leveler holds while it records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeApp {
  /// As `mic_usage::recording_apps` names it (`Zoom.exe`, `MSTeams`).
  pub app: String,
  pub recordings: u32,
  /// Local date of the last one, `YYYY-MM-DD`.
  pub date: String,
  /// Fields a newer version added.
  #[serde(flatten)]
  pub extra: serde_json::Map<String, serde_json::Value>,
}

impl VolumeApp {
  pub fn listed(&self) -> bool {
    self.recordings >= VOLUME_APP_LISTED
  }
}

/// The apps of `VolumeApp`, noted or listed. Missing or unreadable: none.
pub fn volume_apps(dir: &Path) -> Vec<VolumeApp> {
  read_key(dir, VOLUME_APPS_KEY).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

/// One more recording in which `app` moved the mic volume back and forth; its count afterwards.
pub fn note_volume_app(dir: &Path, app: &str) -> u32 {
  let mut apps = volume_apps(dir);
  let date = jiff::ZonedDateTime::now().strftime("%Y-%m-%d").to_string();
  let recordings = match apps.iter_mut().find(|a| a.app.eq_ignore_ascii_case(app)) {
    Some(a) => {
      (a.recordings, a.date) = (a.recordings.saturating_add(1), date);
      a.recordings
    }
    None => {
      apps.push(VolumeApp { app: app.to_string(), recordings: 1, date, extra: Default::default() });
      1
    }
  };
  write_volume_apps(dir, &apps);
  recordings
}

/// The user removed `app` from the list: it starts from nothing again.
pub fn remove_volume_app(dir: &Path, app: &str) {
  let mut apps = volume_apps(dir);
  apps.retain(|a| !a.app.eq_ignore_ascii_case(app));
  write_volume_apps(dir, &apps);
}

fn write_volume_apps(dir: &Path, apps: &[VolumeApp]) {
  match serde_json::to_value(apps) {
    Ok(v) => write_key(dir, VOLUME_APPS_KEY, v),
    Err(e) => log::warn!("[AudioSettings] could not save the mic volume apps: {e}"),
  }
}

/// Windows mic volume (permille) `mic_id` had when the voice leveler was switched off last.
pub fn mic_sensitivity(dir: &Path, mic_id: &str) -> Option<u32> {
  read_key(dir, SENSITIVITY_KEY)?.get(mic_id)?.as_u64().and_then(|v| u32::try_from(v).ok())
}

pub fn set_mic_sensitivity(dir: &Path, mic_id: &str, permille: u32) {
  let mut all = read_key(dir, SENSITIVITY_KEY).filter(|v| v.is_object()).unwrap_or_else(|| serde_json::json!({}));
  if let Some(map) = all.as_object_mut() {
    map.insert(mic_id.to_string(), permille.into());
  }
  write_key(dir, SENSITIVITY_KEY, all);
}

fn read_key(dir: &Path, key: &str) -> Option<serde_json::Value> {
  let bytes = std::fs::read(dir.join(FILE)).ok()?;
  serde_json::from_slice::<serde_json::Value>(&bytes).ok()?.get_mut(key).map(serde_json::Value::take)
}

/// Sets one key, the rest of the file as it is. A corrupt file is kept aside first, as a load does.
/// shortcut: read-modify-write without a lock, so a window saving `AudioSettings` at the same
/// moment can drop this write (the service learns it again); a lock file would close that.
fn write_key(dir: &Path, key: &str, value: serde_json::Value) {
  let path = dir.join(FILE);
  let mut all = match std::fs::read(&path) {
    Ok(bytes) => {
      serde_json::from_slice::<serde_json::Value>(&bytes).ok().filter(|v| v.is_object()).unwrap_or_else(|| {
        log::warn!("[AudioSettings] {} is corrupt, kept aside", path.display());
        crate::output_eq::settings::keep_corrupt(&path);
        serde_json::json!({})
      })
    }
    Err(_) => serde_json::json!({}),
  };
  if let Some(map) = all.as_object_mut() {
    map.insert(key.to_string(), value);
  }
  let result =
    serde_json::to_string_pretty(&all).map_err(std::io::Error::other).and_then(|json| write_atomic(&path, &json));
  if let Err(e) = result {
    log::warn!("[AudioSettings] could not save {key} in {}: {e}", path.display());
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use gamersequalizer_common::shm::DenoiseModel;

  #[test]
  fn the_volume_app_list_lives_beside_the_settings() {
    let dir = std::env::temp_dir().join(format!("ge_audio_settings_volume_apps_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // A file from before the list: no apps, and its keys stay.
    std::fs::write(dir.join(FILE), r#"{"aec_enabled": false}"#).unwrap();
    assert!(volume_apps(&dir).is_empty());
    assert_eq!(note_volume_app(&dir, "MSTeams"), 1);
    assert!(!volume_apps(&dir)[0].listed(), "the first recording only notes it");
    assert_eq!(note_volume_app(&dir, "msteams"), 2);
    assert!(volume_apps(&dir)[0].listed());
    assert!(!AudioSettings::load_or_default(&dir).aec_enabled);
    // A settings save (rebuilt from shared memory, without the list) keeps it.
    AudioSettings::from_header(&ShmHeader::default()).save(&dir);
    assert_eq!(volume_apps(&dir).len(), 1);
    // A field a newer version added to an entry survives a write.
    let text = std::fs::read_to_string(dir.join(FILE))
      .unwrap()
      .replace(r#""recordings": 2"#, r#""recordings": 2, "why": "agc""#);
    std::fs::write(dir.join(FILE), text).unwrap();
    note_volume_app(&dir, "Zoom.exe");
    assert!(std::fs::read_to_string(dir.join(FILE)).unwrap().contains(r#""why": "agc""#));
    remove_volume_app(&dir, "MSTeams");
    assert_eq!(volume_apps(&dir).iter().map(|a| a.app.as_str()).collect::<Vec<_>>(), ["Zoom.exe"]);
    // The sensitivity per mic, beside it.
    set_mic_sensitivity(&dir, "{mic-1}", 620);
    set_mic_sensitivity(&dir, "{mic-2}", 400);
    assert_eq!((mic_sensitivity(&dir, "{mic-1}"), mic_sensitivity(&dir, "{mic-3}")), (Some(620), None));
    assert_eq!(volume_apps(&dir).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
  }

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
    assert_eq!((window.mic.volume, window.mic.applied_volume), (0.53, 0.53));
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
