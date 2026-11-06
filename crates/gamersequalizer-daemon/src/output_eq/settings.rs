//! Output EQ settings, persisted as `%APPDATA%\GamersEqualizer\profiles\output_eq.json`.
//! Missing or corrupt file -> defaults (logged); the app keeps running.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SETTINGS_FILE: &str = "output_eq.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputEqSettings {
  /// Master switch: off = output EQ and the whole mic chain pass audio through.
  pub enabled: bool,
  /// Headphone correction layer on/off (A/B test).
  pub headphone_enabled: bool,
  /// Output endpoint id -> headphone profile name (`profiles/headphones/<name>.json`).
  pub headphone_by_device: BTreeMap<String, String>,
  /// Game profile layer on/off.
  pub game_enabled: bool,
  /// Switch the game profile by the game in the foreground.
  pub auto_detect: bool,
  /// Use launcher metadata (Steam, Epic, GOG, Ubisoft, Microsoft Store) to identify games.
  pub detect_launchers: bool,
  /// Match exe / product / launcher names against profile names.
  pub detect_names: bool,
  /// Game profile used while no game is detected (None = no game EQ).
  pub fallback_game_profile: Option<String>,
  /// exe name (lowercase) -> game profile name. Empty name = "not a game, never switch".
  pub bindings: BTreeMap<String, String>,
  /// Folder with the AutoEQ database (the `results` folder of the AutoEq repository).
  pub autoeq_dir: Option<PathBuf>,
  /// Preamp from the average level (profiles sound equally loud) instead of the highest
  /// boost (never clips, but strong profiles sound quieter).
  pub loudness_match: bool,
  /// Chat channel (Discord & co.): its profile replaces the game profile on those apps'
  /// streams. Off = flat (headphone correction only). Needs the stream EQ APO.
  pub chat_enabled: bool,
  pub chat_profile: Option<String>,
  /// Media channel (browsers, music players), same as chat.
  pub media_enabled: bool,
  pub media_profile: Option<String>,
}

impl Default for OutputEqSettings {
  fn default() -> Self {
    Self {
      enabled: true,
      headphone_enabled: true,
      headphone_by_device: BTreeMap::new(),
      game_enabled: true,
      auto_detect: true,
      detect_launchers: true,
      detect_names: true,
      fallback_game_profile: None,
      bindings: BTreeMap::new(),
      autoeq_dir: None,
      loudness_match: true,
      chat_enabled: true,
      chat_profile: None,
      media_enabled: true,
      media_profile: None,
    }
  }
}

impl OutputEqSettings {
  pub fn load_or_default(path: &Path) -> Self {
    let Ok(text) = std::fs::read_to_string(path) else {
      return Self::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|e| {
      log::warn!("[OutputEq] {} is corrupt ({e}), using defaults", path.display());
      Self::default()
    })
  }

  /// Atomic (temp file + rename) so a crash never leaves half a file.
  pub fn save(&self, path: &Path) -> std::io::Result<()> {
    write_atomic(path, &serde_json::to_string_pretty(self).map_err(std::io::Error::other)?)
  }
}

pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
  if let Some(dir) = path.parent() {
    std::fs::create_dir_all(dir)?;
  }
  let tmp: PathBuf = path.with_extension("json.tmp");
  std::fs::write(&tmp, content)?;
  std::fs::rename(&tmp, path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
  use super::*;

  #[test]
  fn missing_or_corrupt_file_gives_defaults_and_roundtrip_works() {
    let dir = std::env::temp_dir().join(format!("ge_settings_test_{}", std::process::id()));
    let path = dir.join(SETTINGS_FILE);
    assert_eq!(OutputEqSettings::load_or_default(&path), OutputEqSettings::default());

    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&path, "{ not json").unwrap();
    assert_eq!(OutputEqSettings::load_or_default(&path), OutputEqSettings::default());

    let mut s = OutputEqSettings { enabled: false, ..Default::default() };
    s.bindings.insert("division2.exe".into(), "The Division 2".into());
    s.save(&path).unwrap();
    assert_eq!(OutputEqSettings::load_or_default(&path), s);

    // Old file without newer fields still loads.
    std::fs::write(&path, r#"{"enabled": false}"#).unwrap();
    let loaded = OutputEqSettings::load_or_default(&path);
    assert!(!loaded.enabled && loaded.auto_detect);
    let _ = std::fs::remove_dir_all(&dir);
  }
}
