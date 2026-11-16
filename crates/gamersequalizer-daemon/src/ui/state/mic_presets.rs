//! The microphone EQ being edited (`mic_active.json`): loaded infallibly, saved atomically.

use super::*;

/// File holding the microphone EQ the user is currently editing.
const MIC_EQ_FILE: &str = "mic_active.json";

fn mic_profile_path(storage: &ProfileStorage) -> PathBuf {
  storage.base_dir().join(MIC_EQ_FILE)
}

/// Missing/corrupt file -> None (defaults are used), logged.
pub(super) fn load_mic_profile(storage: &ProfileStorage) -> Option<ParametricProfile> {
  let path = mic_profile_path(storage);
  let text = std::fs::read_to_string(&path).ok()?;
  match serde_json::from_str::<ParametricProfile>(&text) {
    Ok(mut p) => {
      // Saved before cuts had a steepness: two stacked rows become one.
      p.bands = collapse_slopes(&p.bands);
      p.bands.iter_mut().for_each(|b| b.clamp_parameters());
      while slots_used(&p.bands) > MAX_EQ_BANDS {
        p.bands.pop();
      }
      Some(p)
    }
    Err(e) => {
      log::warn!("[MicEQ] {} is corrupt ({}), using defaults", path.display(), e);
      crate::output_eq::settings::keep_corrupt(&path);
      None
    }
  }
}

pub(super) fn save_mic_profile(storage: &ProfileStorage, profile: &ParametricProfile) {
  let path = mic_profile_path(storage);
  let result = (|| -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
      std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(profile).map_err(std::io::Error::other)?;
    crate::output_eq::settings::write_atomic(&path, &json)
  })();
  if let Err(e) = result {
    log::warn!("[MicEQ] could not save {}: {}", path.display(), e);
  }
}
