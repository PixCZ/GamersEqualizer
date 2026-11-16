use crate::output_eq::OutputLayer;
use gamersequalizer_common::profile::{ParametricProfile, VadType};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Deleted and replaced profiles are moved here (recoverable), flat: "<name>[.<unix time>].json".
pub const DELETED_DIR: &str = "_deleted";

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

  /// Folder of an output layer's profiles: headphones apart, the others by their category.
  pub fn dir_for(&self, layer: OutputLayer) -> PathBuf {
    layer.vad().map_or_else(|| self.base_dir.join("headphones"), |vad| self.get_vad_directory(vad))
  }

  pub fn layer_profile_path(&self, layer: OutputLayer, name: &str) -> PathBuf {
    self.dir_for(layer).join(format!("{}.json", Self::sanitize_filename(name)))
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

  /// Moves a profile file to `DELETED_DIR` (recoverable). Returns where the file went.
  pub fn move_to_deleted(&self, path: &Path) -> Result<PathBuf, String> {
    let file_name = path.file_name().ok_or("bad profile path")?;
    let dir = self.base_dir.join(DELETED_DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let mut target = dir.join(file_name);
    if target.exists() {
      // Keep older deleted copies: "<name>.json" -> "<name>.<unix time>.json".
      let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
      target = dir.join(format!("{}.{stamp}.json", path.file_stem().unwrap_or_default().to_string_lossy()));
    }
    fs::rename(path, &target).map_err(|e| format!("could not move {}: {e}", path.display()))?;
    log::info!("[Profiles] profile moved to {}", target.display());
    Ok(target)
  }

  /// Save profile as JSON, into the folder of its `vad_type`.
  pub fn save_profile(&self, profile: &ParametricProfile) -> io::Result<PathBuf> {
    write_profile(&self.get_vad_directory(profile.vad_type), profile)
  }

  /// Save into an output layer's folder (a headphone profile's `vad_type` names no folder).
  pub fn save_layer_profile(&self, layer: OutputLayer, profile: &ParametricProfile) -> Result<PathBuf, String> {
    let dir = self.dir_for(layer);
    write_profile(&dir, profile).map_err(|e| format!("could not save {} in {}: {e}", profile.name, dir.display()))
  }

  pub fn load_layer_profiles(&self, layer: OutputLayer) -> Vec<ParametricProfile> {
    self.load_profiles_from_dir(&self.dir_for(layer))
  }

  /// Load all profiles for a given VAD category
  pub fn load_profiles_for_vad(&self, vad: VadType) -> Vec<ParametricProfile> {
    let dir = self.get_vad_directory(vad);
    self.load_profiles_from_dir(&dir)
  }

  pub fn load_profiles_from_dir(&self, dir: &Path) -> Vec<ParametricProfile> {
    let mut result = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
      for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
          match fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|c| serde_json::from_str::<ParametricProfile>(&c).map_err(|e| e.to_string()))
          {
            Ok(mut profile) => {
              sanitize_profile(&mut profile);
              result.push(profile);
            }
            Err(e) => log::warn!("[Profiles] skipping {}: {e}", path.display()),
          }
        }
      }
    }
    result.sort_by_key(|p| p.name.to_lowercase());
    result
  }
}

fn write_profile(dir: &Path, profile: &ParametricProfile) -> io::Result<PathBuf> {
  let path = dir.join(format!("{}.json", ProfileStorage::sanitize_filename(&profile.name)));
  let json = serde_json::to_string_pretty(profile).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
  // Not `File::create` + write: a power cut in between left a 0-byte file, and the profile
  // (and every binding to it) vanished from the UI. `write_atomic` also creates the folder.
  crate::output_eq::settings::write_atomic(&path, &json)?;
  Ok(path)
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
