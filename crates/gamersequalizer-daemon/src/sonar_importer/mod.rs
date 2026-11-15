pub mod converter;
pub mod migrations;
pub mod reader;
pub mod storage;
pub mod winsqlite;

pub use converter::convert_sonar_config;
pub use reader::{DEFAULT_SONAR_DB_PATH, SonarDbReader};
pub use storage::{DELETED_DIR, ProfileStorage};

use crate::output_eq::settings::{OutputEqSettings, SETTINGS_FILE, write_atomic};
use gamersequalizer_common::profile::ParametricProfile;
use log::{info, warn};
use std::collections::BTreeSet;
use std::path::Path;

/// Sonar profiles this app has seen ("game/name"), so a later sync adds only new ones and
/// never brings back one the user deleted or renamed.
pub const KNOWN_FILE: &str = "sonar_known.json";

pub struct ImportStats {
  pub total_imported: usize,
  pub game_presets: usize,
  pub custom_presets: usize,
}

/// Sonar's database and GG's preset scripts at their default places.
pub fn read_sonar_sources() -> Result<Vec<ParametricProfile>, String> {
  SonarDbReader::read_all_sources(Path::new(DEFAULT_SONAR_DB_PATH), &migrations::default_dir())
}

/// Copies every Sonar profile in and remembers them. A file of the same name with other content
/// (the user's edits) is moved to the deleted folder first; when that fails it stays.
pub fn import_sonar(profiles: &[ParametricProfile], storage: &ProfileStorage) -> ImportStats {
  let mut stats = ImportStats { total_imported: 0, game_presets: 0, custom_presets: 0 };
  for profile in profiles {
    let file = format!("{}.json", ProfileStorage::sanitize_filename(&profile.name));
    let path = storage.get_vad_directory(profile.vad_type).join(file);
    if let Ok(old) = std::fs::read_to_string(&path) {
      // The same text `save_profile` writes: an untouched import is not moved.
      if serde_json::to_string_pretty(profile).ok().as_deref() != Some(old.as_str())
        && let Err(e) = storage.move_to_deleted(&path)
      {
        warn!("[Sonar] kept {} (not replaced): {e}", path.display());
        continue;
      }
    }
    match storage.save_profile(profile) {
      Ok(_) => {
        stats.total_imported += 1;
        if profile.vad_type == gamersequalizer_common::profile::VadType::Game {
          stats.game_presets += 1;
        }
        if profile.is_custom {
          stats.custom_presets += 1;
        }
      }
      Err(e) => warn!("[Sonar] could not save {}: {e}", profile.name),
    }
  }
  let mut known = load_known(storage);
  known.extend(profiles.iter().map(profile_key));
  save_known(storage, &known);
  stats
}

/// After an import: adds the Sonar profiles that appeared since (a GG update brings new
/// presets). Skips names the app has seen, has a file for, or has in the deleted folder.
/// Returns how many it added.
/// shortcut: new game ids of a preset the user already has are not taken over (that would
/// overwrite their copy); "Import from Sonar DB" refreshes everything.
pub fn sync_new_sonar_profiles(storage: &ProfileStorage) -> usize {
  let known_path = storage.base_dir().join(KNOWN_FILE);
  let imported = OutputEqSettings::load_or_default(&storage.base_dir().join(SETTINGS_FILE)).imported_sonar;
  if !imported && !known_path.exists() {
    return 0; // never imported: the setup card / button offers it
  }
  let profiles = match read_sonar_sources() {
    Ok(p) => p,
    Err(e) => {
      info!("[Sonar] no new profiles checked: {e}");
      return 0;
    }
  };
  sync_new(&profiles, storage)
}

fn sync_new(profiles: &[ParametricProfile], storage: &ProfileStorage) -> usize {
  let mut known = load_known(storage);
  let deleted = deleted_names(storage);
  let mut added = 0;
  for profile in profiles {
    let file = ProfileStorage::sanitize_filename(&profile.name);
    if known.contains(&profile_key(profile))
      || storage.get_vad_directory(profile.vad_type).join(format!("{file}.json")).exists()
      || deleted.contains(&file.to_lowercase())
    {
      continue;
    }
    match storage.save_profile(profile) {
      Ok(path) => {
        info!("[Sonar] new profile added: {}", path.display());
        added += 1;
      }
      Err(e) => warn!("[Sonar] could not save new profile {}: {e}", profile.name),
    }
  }
  let before = known.len();
  known.extend(profiles.iter().map(profile_key));
  if known.len() != before {
    save_known(storage, &known);
  }
  added
}

/// "game/<file name>" in lowercase: one file per channel and name (NTFS ignores case).
fn profile_key(p: &ParametricProfile) -> String {
  format!("{}/{}", p.vad_type.as_str(), ProfileStorage::sanitize_filename(&p.name).to_lowercase())
}

fn load_known(storage: &ProfileStorage) -> BTreeSet<String> {
  let path = storage.base_dir().join(KNOWN_FILE);
  match std::fs::read_to_string(&path) {
    Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
      warn!("[Sonar] {} is corrupt ({e}), starting over", path.display());
      BTreeSet::new()
    }),
    Err(_) => BTreeSet::new(),
  }
}

fn save_known(storage: &ProfileStorage, known: &BTreeSet<String>) {
  let path = storage.base_dir().join(KNOWN_FILE);
  let saved = serde_json::to_string_pretty(known).map_err(std::io::Error::other).and_then(|t| write_atomic(&path, t));
  if let Err(e) = saved {
    warn!("[Sonar] could not save {}: {e}", path.display());
  }
}

/// Lowercase file names in the deleted folder, without ".json" and the ".<unix time>" suffix.
fn deleted_names(storage: &ProfileStorage) -> BTreeSet<String> {
  let dir = storage.base_dir().join(DELETED_DIR);
  let mut names = BTreeSet::new();
  for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
    let name = entry.file_name().to_string_lossy().to_lowercase();
    let Some(stem) = name.strip_suffix(".json") else { continue };
    let stem = match stem.rsplit_once('.') {
      Some((base, stamp)) if !stamp.is_empty() && stamp.bytes().all(|b| b.is_ascii_digit()) => base,
      _ => stem,
    };
    names.insert(stem.to_string());
  }
  names
}

#[cfg(test)]
mod tests {
  use super::*;
  use gamersequalizer_common::profile::VadType;

  #[test]
  fn sync_adds_only_new_profiles_and_never_restores_deleted_ones() {
    let dir = std::env::temp_dir().join(format!("ge_sonar_sync_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let storage = ProfileStorage::new(&dir);
    let p = |name: &str| ParametricProfile::new(name, VadType::Game);

    assert_eq!(import_sonar(&[p("Kept"), p("Deleted"), p("Renamed")], &storage).total_imported, 3);
    let game = storage.get_vad_directory(VadType::Game);
    std::fs::create_dir_all(dir.join(DELETED_DIR)).unwrap();
    std::fs::rename(game.join("Deleted.json"), dir.join(DELETED_DIR).join("Deleted.json")).unwrap();
    std::fs::rename(game.join("Renamed.json"), game.join("My name.json")).unwrap();
    std::fs::write(game.join("Kept.json"), "{\"edited\": true}").unwrap();

    // A GG update: one new preset; one the user deleted before this app tracked names.
    std::fs::write(dir.join(DELETED_DIR).join("Old gone.1700000000.json"), "{}").unwrap();
    let after_update = [p("Kept"), p("Deleted"), p("Renamed"), p("New one"), p("Old gone")];
    assert_eq!(sync_new(&after_update, &storage), 1);
    assert!(game.join("New one.json").exists());
    assert!(!game.join("Deleted.json").exists() && !game.join("Renamed.json").exists());
    assert!(!game.join("Old gone.json").exists());
    assert_eq!(std::fs::read_to_string(game.join("Kept.json")).unwrap(), "{\"edited\": true}", "the user's copy stays");
    assert_eq!(sync_new(&after_update, &storage), 0, "nothing new the second time");
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn import_moves_a_changed_profile_to_the_deleted_folder_first() {
    let dir = std::env::temp_dir().join(format!("ge_sonar_import_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let storage = ProfileStorage::new(&dir);
    let a = ParametricProfile::new("Apex", VadType::Game);
    import_sonar(std::slice::from_ref(&a), &storage);
    let file = storage.get_vad_directory(VadType::Game).join("Apex.json");
    let sonar = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, "{\"edited\": true}").unwrap();

    import_sonar(std::slice::from_ref(&a), &storage);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), sonar, "Sonar's version is in place");
    let deleted = dir.join(DELETED_DIR).join("Apex.json");
    assert_eq!(std::fs::read_to_string(&deleted).unwrap(), "{\"edited\": true}", "the user's copy is kept");

    import_sonar(std::slice::from_ref(&a), &storage);
    assert_eq!(std::fs::read_dir(dir.join(DELETED_DIR)).unwrap().count(), 1, "an unchanged profile is not moved");
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn sync_waits_for_a_first_import() {
    let dir = std::env::temp_dir().join(format!("ge_sonar_sync_none_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(sync_new_sonar_profiles(&ProfileStorage::new(&dir)), 0);
    assert!(!dir.join(KNOWN_FILE).exists());
    let _ = std::fs::remove_dir_all(&dir);
  }
}
