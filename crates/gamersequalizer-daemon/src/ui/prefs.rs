//! Appearance preferences (language, light/dark theme) in
//! `%APPDATA%\GamersEqualizer\profiles\ui_prefs.json`. Missing or corrupt file -> defaults
//! (follow Windows), logged; the app keeps running.

use crate::i18n::LanguagePref;
use crate::output_eq::settings::write_atomic;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FILE: &str = "ui_prefs.json";

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
  #[serde(skip)]
  path: PathBuf,
}

impl UiPrefs {
  pub fn load_or_default(dir: &Path) -> Self {
    let path = dir.join(FILE);
    let mut prefs = match std::fs::read_to_string(&path) {
      Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
        log::warn!("[Prefs] {} is corrupt ({e}), using defaults", path.display());
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
#[allow(clippy::unwrap_used)]
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
    p.save();
    let loaded = UiPrefs::load_or_default(&dir);
    assert_eq!((loaded.language, loaded.theme), (LanguagePref::Czech, ThemePref::Light));

    // A value from a newer version falls back instead of resetting everything.
    std::fs::write(dir.join(FILE), r#"{"language":"Czech","theme":"HighContrast"}"#).unwrap();
    let loaded = UiPrefs::load_or_default(&dir);
    assert_eq!((loaded.language, loaded.theme), (LanguagePref::Czech, ThemePref::System));
    let _ = std::fs::remove_dir_all(&dir);
  }
}
