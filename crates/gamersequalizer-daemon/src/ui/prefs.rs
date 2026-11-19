//! Appearance preferences (language, light/dark theme) in
//! `%APPDATA%\GamersEqualizer\profiles\ui_prefs.json`. Missing or corrupt file -> defaults
//! (follow Windows), logged; the app keeps running.

use crate::i18n::LanguagePref;
use crate::output_eq::settings::write_atomic;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub(crate) const FILE: &str = "ui_prefs.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThemePref {
  Light,
  Dark,
  /// Light or dark like Windows ("Choose your mode" in Settings > Personalization > Colors).
  /// Last: serde's catch-all for values from newer versions.
  #[default]
  #[serde(other)]
  System,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPrefs {
  pub language: LanguagePref,
  pub theme: ThemePref,
  /// The first-run setup card was answered (set up or "Not now"); it no longer opens by itself.
  pub setup_done: bool,
  /// The user explicitly unchecked Output EQ when applying setup.
  pub output_declined: bool,
  /// Endpoints the installer rolled back (`apo_install::endpoint_key`): the setup card no longer
  /// offers them until the user clicks "Try again".
  pub rolled_back: Vec<String>,
  /// Time of the newest installer rollback line already taken into `rolled_back` (ISO UTC, as in
  /// the note), so a line still in the notes does not bring back a device the user retried.
  pub rolled_back_seen: String,
  /// The tray icon's bars stand still instead of following the microphone level (`tray_level`;
  /// the service reads it from this file while an app records).
  pub tray_mic_level_off: bool,
  /// Fields from a newer version of the app, kept so a save here does not drop them.
  #[serde(flatten)]
  pub extra: serde_json::Map<String, serde_json::Value>,
  #[serde(skip)]
  path: PathBuf,
}

impl UiPrefs {
  pub fn load_or_default(dir: &Path) -> Self {
    let path = dir.join(FILE);
    let mut prefs = match std::fs::read_to_string(&path) {
      Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
        log::warn!("[Prefs] {} is corrupt ({e}), using defaults", path.display());
        crate::output_eq::settings::keep_corrupt(&path);
        Self::default()
      }),
      Err(_) => Self::default(),
    };
    prefs.path = path;
    prefs
  }

  pub fn save(&self) {
    let result = serde_json::to_string_pretty(self)
      .map_err(std::io::Error::other)
      .and_then(|json| write_atomic(&self.path, &json));
    if let Err(e) = result {
      log::warn!("[Prefs] could not save {}: {e}", self.path.display());
    }
  }

  pub fn egui_theme(&self) -> egui::ThemePreference {
    match self.theme {
      ThemePref::System => egui::ThemePreference::System,
      ThemePref::Light => egui::ThemePreference::Light,
      ThemePref::Dark => egui::ThemePreference::Dark,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn missing_corrupt_and_roundtrip() {
    let dir = std::env::temp_dir().join(format!("ge_prefs_test_{}", std::process::id()));
    assert_eq!(UiPrefs::load_or_default(&dir).theme, ThemePref::System);

    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(FILE), "{ nope").unwrap();
    assert_eq!(UiPrefs::load_or_default(&dir).language, LanguagePref::System);

    let mut p = UiPrefs::load_or_default(&dir);
    p.language = LanguagePref::Czech;
    p.theme = ThemePref::Light;
    p.output_declined = true;
    p.rolled_back = vec!["Capture:{abc}".to_string()];
    p.save();
    let loaded = UiPrefs::load_or_default(&dir);
    assert_eq!((loaded.language, loaded.theme, loaded.output_declined), (LanguagePref::Czech, ThemePref::Light, true));
    assert_eq!(loaded.rolled_back, ["Capture:{abc}"]);

    // Existing files without `output_declined` default to false, and unknown enum values fall back.
    std::fs::write(dir.join(FILE), r#"{"language":"Czech","theme":"HighContrast","setup_done":true}"#).unwrap();
    let loaded = UiPrefs::load_or_default(&dir);
    assert_eq!(
      (loaded.language, loaded.theme, loaded.setup_done, loaded.output_declined, loaded.tray_mic_level_off),
      (LanguagePref::Czech, ThemePref::System, true, false, false)
    );
    let _ = std::fs::remove_dir_all(&dir);
  }
}
