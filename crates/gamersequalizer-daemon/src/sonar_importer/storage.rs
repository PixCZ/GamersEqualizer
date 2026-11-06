use gamersequalizer_common::profile::{ParametricProfile, VadType};
use std::fs::{self, File};
use std::io::{self, Write};
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

    let mut file = File::create(&path)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;

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
          if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(profile) = serde_json::from_str::<ParametricProfile>(&content) {
              result.push(profile);
            }
          }
        }
      }
    }
    result.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    result
  }
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
    let tmp = std::env::temp_dir().join("opensonar_test_storage");
    let storage = ProfileStorage::new(&tmp);

    let profile = ParametricProfile::new("CS:GO / CS2 Pro", VadType::Game);
    let path = storage.save_profile(&profile).unwrap();
    assert!(path.exists());

    let loaded = storage.load_profiles_for_vad(VadType::Game);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].name, "CS:GO / CS2 Pro");

    let _ = fs::remove_dir_all(&tmp);
  }
}
