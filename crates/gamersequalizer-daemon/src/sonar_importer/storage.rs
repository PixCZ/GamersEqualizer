use gamersequalizer_common::profile::{ParametricProfile, VadType};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ProfileStorage {
  base_dir: PathBuf,
}

impl ProfileStorage {
  pub fn default_storage() -> Self {
    let app_data = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));

    let legacy_dir = app_data.join("OpenSonar").join("profiles");
    let base_dir = app_data.join("GamersEqualizer").join("profiles");

    // Seamless migration from legacy directory if present
    if legacy_dir.exists() && !base_dir.exists() {
      let _ = fs::create_dir_all(&base_dir);
      let _ = copy_dir_all(&legacy_dir, &base_dir);
    }

    Self { base_dir }
  }

  pub fn new(base_dir: impl Into<PathBuf>) -> Self {
    Self { base_dir: base_dir.into() }
  }

  pub fn base_dir(&self) -> &Path {
    &self.base_dir
  }

  pub fn get_vad_directory(&self, vad: VadType) -> PathBuf {
    self.base_dir.join(vad.as_str())
  }

  pub fn get_headphones_directory(&self) -> PathBuf {
    self.base_dir.join("headphones")
  }

  /// Sanitize filename for Windows NTFS
  pub fn sanitize_filename(name: &str) -> String {
    let mut clean: String = name
      .chars()
      .map(|c| match c {
        ':' | '/' | '\\' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
        other => other,
      })
      .collect();

    if clean.trim().is_empty() {
      clean = "unnamed_profile".to_string();
    }
    clean
  }

  /// Save profile as JSON
  pub fn save_profile(&self, profile: &ParametricProfile) -> io::Result<PathBuf> {
    let dir = self.get_vad_directory(profile.vad_type);
    fs::create_dir_all(&dir)?;

    let filename = format!("{}.json", Self::sanitize_filename(&profile.name));
    let path = dir.join(filename);

    let json = serde_json::to_string_pretty(profile).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    // Not `File::create` + write: a power cut in between left a 0-byte file, and the profile
    // (and every binding to it) vanished from the UI.
    crate::output_eq::settings::write_atomic(&path, &json)?;
    Ok(path)
  }

  /// Load all profiles for a given VAD category
  pub fn load_profiles_for_vad(&self, vad: VadType) -> Vec<ParametricProfile> {
    let dir = self.get_vad_directory(vad);
    self.load_profiles_from_dir(&dir)
  }

  /// Load headphone calibration profiles
  pub fn load_headphone_profiles(&self) -> Vec<ParametricProfile> {
    let dir = self.get_headphones_directory();
    self.load_profiles_from_dir(&dir)
  }

  pub fn load_profiles_from_dir(&self, dir: &Path) -> Vec<ParametricProfile> {
    let mut result = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
      for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
          match fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|c| {
            serde_json::from_str::<ParametricProfile>(&c).map_err(|e| e.to_string())
          }) {
            Ok(mut profile) => {
              sanitize_profile(&mut profile);
              result.push(profile);
            }
            Err(e) => log::warn!("[Profiles] skipping {}: {e}", path.display()),
          }
        }
      }
    }
    result.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    result
  }
}

/// Trust boundary: profiles are user-editable files ("gain_db": 1e39 parses as inf and went
/// straight to the APO). Same ranges as the editor.
fn sanitize_profile(profile: &mut ParametricProfile) {
  for band in &mut profile.bands {
    band.clamp_parameters();
  }
  let finite_or_zero = |v: f32, lo: f32, hi: f32| if v.is_finite() { v.clamp(lo, hi) } else { 0.0 };
  profile.preamp_db = finite_or_zero(profile.preamp_db, -48.0, 24.0);
  profile.gain_db = finite_or_zero(profile.gain_db, -24.0, 24.0);
}

fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
  if !dst.exists() {
    fs::create_dir_all(dst)?;
  }
  for entry in fs::read_dir(src)? {
    let entry = entry?;
    let ty = entry.file_type()?;
    let dest_path = dst.join(entry.file_name());
    if ty.is_dir() {
      copy_dir_all(&entry.path(), &dest_path)?;
    } else {
      let _ = fs::copy(entry.path(), dest_path);
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_save_and_load_profile() {
    let tmp = std::env::temp_dir().join("ge_test_storage");
    let storage = ProfileStorage::new(&tmp);

    let profile = ParametricProfile::new("CS:GO / CS2 Pro", VadType::Game);
    let path = storage.save_profile(&profile).unwrap();
    assert!(path.exists());

    let loaded = storage.load_profiles_for_vad(VadType::Game);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].name, "CS:GO / CS2 Pro");

    let _ = fs::remove_dir_all(&tmp);
  }

  #[test]
  fn hand_edited_out_of_range_values_are_clamped_on_load() {
    let tmp = std::env::temp_dir().join(format!("ge_test_storage_clamp_{}", std::process::id()));
    let storage = ProfileStorage::new(&tmp);
    let mut profile = ParametricProfile::new("Edited", VadType::Game);
    profile.bands.push(gamersequalizer_common::profile::BiquadBand::default());
    let path = storage.save_profile(&profile).unwrap();
    let text = fs::read_to_string(&path).unwrap().replacen("\"gain_db\": 0.0", "\"gain_db\": 1e39", 2);
    fs::write(&path, text).unwrap();

    let loaded = storage.load_profiles_for_vad(VadType::Game);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].bands[0].gain_db, 24.0);
    assert_eq!(loaded[0].gain_db, 0.0);
    let _ = fs::remove_dir_all(&tmp);
  }
}
