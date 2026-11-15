//! Export / import of the user's settings and profiles, for a reinstalled or a new PC.
//!
//! One readable JSON file, gzip-compressed when its name ends in `.gz` (the default):
//! ```json
//! {
//!   "format": "GamersEqualizer settings",
//!   "format_version": 1,
//!   "exported_at": "2026-11-15 14:02:11 +01:00",
//!   "app_version": "0.1.2",
//!   "devices": { "{0.0.0.00000000}.{b45c...}": "Headphones (EPOS GSA 70)" },
//!   "settings": { "output_eq": { ... }, "audio_settings": { ... }, ... },
//!   "profiles": { "game": { "Apex Legends": { ...profile... } }, "headphones": { ... } }
//! }
//! ```
//! Device ids differ on another PC; `devices` carries their names, and the service moves the
//! settings to the same-named devices there (`win_audio::device_ids`).
//! Not exported: logs, caches, window position, setup state (the effects are installed per PC).
//! Export and import can be limited to some parts (`Part`): e.g. only the game profiles for a
//! friend, whose microphone settings must stay as they are.

use crate::output_eq::settings::write_atomic;
use crate::win_audio::device_ids::{self, KnownDevices};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use gamersequalizer_common::profile::ParametricProfile;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const FORMAT: &str = "GamersEqualizer settings";
const FORMAT_VERSION: u32 = 1;
/// Settings files (stem, in the profiles folder) that move to another PC.
const SETTINGS: [&str; 6] = ["output_eq", "audio_settings", "sidetone", "app_channels", "mic_active", "ui_prefs"];
/// From `ui_prefs` only these move; the rest is setup state of this PC.
const UI_PREFS_KEYS: [&str; 2] = ["language", "theme"];
/// A real export is a few MB; anything far bigger is not ours. Also the limit after unpacking.
const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024;
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const BACKUP_DIR: &str = "backups";

#[derive(Debug, Serialize, Deserialize)]
struct ExportFile {
  format: String,
  format_version: u32,
  #[serde(default)]
  exported_at: String,
  #[serde(default)]
  app_version: String,
  #[serde(default)]
  devices: KnownDevices,
  #[serde(default)]
  settings: BTreeMap<String, serde_json::Value>,
  /// Folder -> file name (without .json) -> profile.
  #[serde(default)]
  profiles: BTreeMap<String, BTreeMap<String, ParametricProfile>>,
}

/// One part the user can leave out: `profiles/<folder>` or `settings/<stem>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
  pub key: String,
  /// Profiles in the folder; 0 for a settings part.
  pub profiles: usize,
}

/// Keys of the parts to export or import.
pub type Selection = BTreeSet<String>;

/// What a file holds, shown before the import is confirmed.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
  pub path: PathBuf,
  pub exported_at: String,
  pub profiles: usize,
  pub parts: Vec<Part>,
  pub devices: Vec<String>,
}

/// The parts this PC has, for the export checkboxes.
pub fn parts_here(profiles_dir: &Path) -> Vec<Part> {
  parts(&collect(profiles_dir))
}

/// Writes the selected parts (`None`: everything) to `out`.
pub fn export(profiles_dir: &Path, out: &Path, only: Option<&Selection>) -> Result<Summary, String> {
  let mut file = collect(profiles_dir);
  if let Some(only) = only {
    keep(&mut file, only);
  }
  let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
  let bytes = if is_gz(out) { gzip(&text).map_err(|e| e.to_string())? } else { text.into_bytes() };
  write_atomic(out, bytes).map_err(|e| format!("{}: {e}", out.display()))?;
  info!(
    "[Transfer] exported {} profiles and {} settings files to {}",
    count(&file),
    file.settings.len(),
    out.display()
  );
  Ok(summary(out, &file))
}

/// Reads and checks `path` without changing anything.
pub fn inspect(path: &Path) -> Result<Summary, String> {
  read(path).map(|file| summary(path, &file))
}

/// Backs the current state up, then writes the selected parts of the file (`None`: all).
/// Settings files are replaced; profiles are added (a profile of the same name is replaced,
/// others stay); parts left out stay as they are here.
/// Run only while no other process of the app writes settings (window closed, service ended).
pub fn import(profiles_dir: &Path, path: &Path, only: Option<&Selection>) -> Result<PathBuf, String> {
  let mut file = read(path)?;
  if let Some(only) = only {
    keep(&mut file, only);
  }
  let backup = backup_path(profiles_dir);
  export(profiles_dir, &backup, None).map_err(|e| format!("backup before import failed: {e}"))?;

  let mut errors = Vec::new();
  for (dir, profiles) in &file.profiles {
    let folder = profiles_dir.join(dir);
    if let Err(e) = std::fs::create_dir_all(&folder) {
      errors.push(format!("{}: {e}", folder.display()));
      continue;
    }
    for (name, profile) in profiles {
      let saved = serde_json::to_string_pretty(profile)
        .map_err(std::io::Error::other)
        .and_then(|text| write_atomic(&folder.join(format!("{name}.json")), &text));
      if let Err(e) = saved {
        errors.push(format!("{dir}/{name}: {e}"));
      }
    }
  }
  for (stem, value) in &file.settings {
    let target = profiles_dir.join(format!("{stem}.json"));
    let value = if *stem == "ui_prefs" { merged_ui_prefs(&target, value) } else { value.clone() };
    let saved =
      serde_json::to_string_pretty(&value).map_err(std::io::Error::other).and_then(|text| write_atomic(&target, &text));
    if let Err(e) = saved {
      errors.push(format!("{stem}: {e}"));
    }
  }
  // Names of the old PC's devices, so the service finds them here under their new ids.
  let mut known = device_ids::load_known(profiles_dir);
  for (id, name) in file.devices {
    known.entry(id).or_insert(name);
  }
  device_ids::save_known(profiles_dir, &known);

  if errors.is_empty() {
    info!("[Transfer] imported {} (backup of the previous state: {})", path.display(), backup.display());
    Ok(backup)
  } else {
    Err(format!(
      "{} of the files could not be written ({}); backup: {}",
      errors.len(),
      errors.join("; "),
      backup.display()
    ))
  }
}

/// Suggested file name: `GamersEqualizer-settings-2026-11-15.json.gz`.
pub fn default_file_name() -> String {
  format!("GamersEqualizer-settings-{}.json.gz", jiff::Zoned::now().strftime("%Y-%m-%d"))
}

// --- private helpers -----------------------------------------------------------------------

/// Drops the parts not in `only`. Device names go only with a settings file that stores ids:
/// profiles shared with a friend carry no device list.
fn keep(file: &mut ExportFile, only: &Selection) {
  file.settings.retain(|stem, _| only.contains(&format!("settings/{stem}")));
  file.profiles.retain(|dir, _| only.contains(&format!("profiles/{dir}")));
  if !file.settings.keys().any(|stem| device_ids::FILES_WITH_IDS.contains(&format!("{stem}.json").as_str())) {
    file.devices.clear();
  }
}

fn parts(file: &ExportFile) -> Vec<Part> {
  let profiles = file.profiles.iter().map(|(dir, p)| Part { key: format!("profiles/{dir}"), profiles: p.len() });
  let settings = file.settings.keys().map(|stem| Part { key: format!("settings/{stem}"), profiles: 0 });
  profiles.chain(settings).collect()
}

fn collect(profiles_dir: &Path) -> ExportFile {
  let mut settings = BTreeMap::new();
  for stem in SETTINGS {
    let path = profiles_dir.join(format!("{stem}.json"));
    let Ok(text) = std::fs::read_to_string(&path) else { continue };
    match serde_json::from_str::<serde_json::Value>(&text) {
      Ok(mut value) => {
        if stem == "ui_prefs" {
          value = only_keys(&value, &UI_PREFS_KEYS);
        }
        settings.insert(stem.to_string(), value);
      }
      Err(e) => warn!("[Transfer] {} skipped, not valid JSON: {e}", path.display()),
    }
  }
  let mut profiles = BTreeMap::new();
  for entry in std::fs::read_dir(profiles_dir).into_iter().flatten().flatten() {
    let dir = entry.file_name().to_string_lossy().into_owned();
    if !entry.path().is_dir() || !is_safe_dir(&dir) {
      continue;
    }
    let mut in_dir = BTreeMap::new();
    for file in std::fs::read_dir(entry.path()).into_iter().flatten().flatten() {
      let path = file.path();
      let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
      if path.extension().and_then(|e| e.to_str()) != Some("json") || !is_safe_name(&stem) {
        continue;
      }
      let parsed = std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str::<ParametricProfile>(&t).map_err(|e| e.to_string()));
      match parsed {
        Ok(p) => {
          in_dir.insert(stem, p);
        }
        Err(e) => warn!("[Transfer] {} skipped: {e}", path.display()),
      }
    }
    if !in_dir.is_empty() {
      profiles.insert(dir, in_dir);
    }
  }
  ExportFile {
    format: FORMAT.into(),
    format_version: FORMAT_VERSION,
    exported_at: jiff::Zoned::now().strftime("%Y-%m-%d %H:%M:%S %:z").to_string(),
    app_version: env!("GAMERSEQUALIZER_VERSION").into(),
    devices: device_ids::load_known(profiles_dir),
    settings,
    profiles,
  }
}

/// Trust boundary: the file may come from anywhere. All or nothing: any bad part rejects it.
fn read(path: &Path) -> Result<ExportFile, String> {
  let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
  if size > MAX_IMPORT_BYTES {
    return Err(format!("the file is too big ({size} bytes) to be a settings export"));
  }
  let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
  parse(&unpack(bytes)?)
}

/// The file's text: gunzipped when it starts like gzip (whatever its name), else as it is.
fn unpack(bytes: Vec<u8>) -> Result<String, String> {
  if !bytes.starts_with(&GZIP_MAGIC) {
    return String::from_utf8(bytes).map_err(|_| "not a GamersEqualizer settings file".to_string());
  }
  let mut text = String::new();
  // One byte over the limit tells a too big file from one that fits exactly.
  GzDecoder::new(bytes.as_slice())
    .take(MAX_IMPORT_BYTES + 1)
    .read_to_string(&mut text)
    .map_err(|e| format!("the compressed file is damaged: {e}"))?;
  if text.len() as u64 > MAX_IMPORT_BYTES {
    return Err("the file unpacks to more than a settings export can be".into());
  }
  Ok(text)
}

fn gzip(text: &str) -> std::io::Result<Vec<u8>> {
  let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
  encoder.write_all(text.as_bytes())?;
  encoder.finish()
}

fn is_gz(path: &Path) -> bool {
  path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gz"))
}

fn parse(text: &str) -> Result<ExportFile, String> {
  let file: ExportFile = serde_json::from_str(text).map_err(|e| format!("not a GamersEqualizer settings file: {e}"))?;
  if file.format != FORMAT {
    return Err("not a GamersEqualizer settings file".into());
  }
  if file.format_version > FORMAT_VERSION {
    return Err(format!("made by a newer version of the app (format {}); update the app first", file.format_version));
  }
  for (stem, value) in &file.settings {
    if !SETTINGS.contains(&stem.as_str()) {
      return Err(format!("unknown settings part \"{stem}\""));
    }
    if !value.is_object() {
      return Err(format!("settings part \"{stem}\" is not an object"));
    }
  }
  for (dir, profiles) in &file.profiles {
    if !is_safe_dir(dir) {
      return Err(format!("invalid profile folder \"{dir}\""));
    }
    if let Some(bad) = profiles.keys().find(|n| !is_safe_name(n)) {
      return Err(format!("invalid profile file name \"{bad}\""));
    }
  }
  Ok(file)
}

/// A profile folder: letters, digits, `_`, `-`; not `_deleted` and the like.
fn is_safe_dir(name: &str) -> bool {
  !name.is_empty()
    && name.len() <= 64
    && !name.starts_with('_')
    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A file name Windows accepts and that stays inside its folder.
fn is_safe_name(name: &str) -> bool {
  !name.is_empty()
    && name.chars().count() <= 200
    && !name.starts_with(['.', ' '])
    && !name.ends_with(['.', ' '])
    && !name.chars().any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c))
}

fn only_keys(value: &serde_json::Value, keys: &[&str]) -> serde_json::Value {
  let map: serde_json::Map<String, serde_json::Value> =
    keys.iter().filter_map(|k| value.get(*k).map(|v| (k.to_string(), v.clone()))).collect();
  serde_json::Value::Object(map)
}

/// This PC's `ui_prefs` with the imported language and theme.
fn merged_ui_prefs(target: &Path, imported: &serde_json::Value) -> serde_json::Value {
  let mut current = std::fs::read_to_string(target)
    .ok()
    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    .filter(serde_json::Value::is_object)
    .unwrap_or_else(|| serde_json::json!({}));
  if let (Some(map), serde_json::Value::Object(new)) = (current.as_object_mut(), only_keys(imported, &UI_PREFS_KEYS)) {
    map.extend(new);
  }
  current
}

fn backup_path(profiles_dir: &Path) -> PathBuf {
  let dir = profiles_dir.parent().unwrap_or(profiles_dir).join(BACKUP_DIR);
  dir.join(format!("before-import-{}.json.gz", jiff::Zoned::now().strftime("%Y%m%d-%H%M%S")))
}

fn count(file: &ExportFile) -> usize {
  file.profiles.values().map(BTreeMap::len).sum()
}

fn summary(path: &Path, file: &ExportFile) -> Summary {
  Summary {
    path: path.to_path_buf(),
    exported_at: file.exported_at.clone(),
    profiles: count(file),
    parts: parts(file),
    devices: file.devices.values().cloned().collect(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use gamersequalizer_common::profile::VadType;

  fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ge_transfer_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    dir.join("profiles")
  }

  fn write_profile(dir: &Path, folder: &str, name: &str) {
    std::fs::create_dir_all(dir.join(folder)).unwrap();
    let p = ParametricProfile::new(name, VadType::Game);
    std::fs::write(dir.join(folder).join(format!("{name}.json")), serde_json::to_string(&p).unwrap()).unwrap();
  }

  #[test]
  fn export_then_import_on_another_pc() {
    let old_pc = temp("old");
    write_profile(&old_pc, "game", "Apex Legends");
    write_profile(&old_pc, "headphones", "EPOS H6PRO");
    write_profile(&old_pc, "_deleted", "Gone");
    std::fs::write(old_pc.join("output_eq.json"), r#"{"headphone_by_device":{"{old-epos}":"EPOS H6PRO"}}"#).unwrap();
    std::fs::write(old_pc.join("ui_prefs.json"), r#"{"language":"Czech","theme":"Dark","setup_done":true}"#).unwrap();
    std::fs::write(old_pc.join("window.json"), r#"{"x":1}"#).unwrap();
    std::fs::write(old_pc.join("devices.json"), r#"{"{old-epos}":"Headphones (EPOS GSA 70)"}"#).unwrap();
    let file = old_pc.parent().unwrap().join("export.json");
    let s = export(&old_pc, &file, None).unwrap();
    assert_eq!((s.profiles, s.devices.clone()), (2, vec!["Headphones (EPOS GSA 70)".to_string()]), "_deleted stays");
    assert!(!s.parts.iter().any(|p| p.key == "settings/window"));

    let new_pc = temp("new");
    write_profile(&new_pc, "game", "My own");
    std::fs::write(new_pc.join("ui_prefs.json"), r#"{"language":"English","setup_done":false}"#).unwrap();
    assert_eq!(inspect(&file).unwrap().profiles, 2);
    let backup = import(&new_pc, &file, None).unwrap();

    assert!(backup.exists(), "previous state backed up");
    assert!(new_pc.join("game").join("Apex Legends.json").exists());
    assert!(new_pc.join("game").join("My own.json").exists(), "own profiles stay");
    assert!(new_pc.join("output_eq.json").exists());
    let prefs: serde_json::Value =
      serde_json::from_str(&std::fs::read_to_string(new_pc.join("ui_prefs.json")).unwrap()).unwrap();
    assert_eq!(
      prefs,
      serde_json::json!({"language":"Czech","theme":"Dark","setup_done":false}),
      "setup state of this PC kept"
    );
    assert_eq!(device_ids::load_known(&new_pc).get("{old-epos}").map(String::as_str), Some("Headphones (EPOS GSA 70)"));
    let _ = std::fs::remove_dir_all(old_pc.parent().unwrap());
    let _ = std::fs::remove_dir_all(new_pc.parent().unwrap());
  }

  #[test]
  fn only_the_selected_parts_move() {
    let mine = temp("mine");
    write_profile(&mine, "game", "Apex Legends");
    write_profile(&mine, "mic", "My voice");
    std::fs::write(mine.join("audio_settings.json"), r#"{"mic_device_id":"{my-mic}"}"#).unwrap();
    std::fs::write(mine.join("devices.json"), r#"{"{my-mic}":"Mic (USB)"}"#).unwrap();
    let file = mine.parent().unwrap().join("for-a-friend.json");
    let only: Selection = ["profiles/game".to_string()].into();
    let s = export(&mine, &file, Some(&only)).unwrap();
    assert_eq!(s.parts, [Part { key: "profiles/game".into(), profiles: 1 }]);
    assert!(s.devices.is_empty(), "no device names without the settings that use them");

    // The friend imports everything from a full file but keeps their microphone settings.
    let full = mine.parent().unwrap().join("full.json");
    export(&mine, &full, None).unwrap();
    let friend = temp("friend");
    std::fs::write(friend.join("audio_settings.json"), r#"{"mic_device_id":"{friend-mic}"}"#).unwrap();
    let only: Selection = ["profiles/game".to_string(), "profiles/mic".to_string()].into();
    import(&friend, &full, Some(&only)).unwrap();
    assert!(friend.join("game").join("Apex Legends.json").exists());
    assert!(friend.join("mic").join("My voice.json").exists());
    assert_eq!(
      std::fs::read_to_string(friend.join("audio_settings.json")).unwrap(),
      r#"{"mic_device_id":"{friend-mic}"}"#
    );
    let _ = std::fs::remove_dir_all(mine.parent().unwrap());
    let _ = std::fs::remove_dir_all(friend.parent().unwrap());
  }

  #[test]
  fn gzip_and_plain_files_both_import() {
    let dir = temp("gz");
    write_profile(&dir, "game", "Apex Legends");
    let gz = dir.parent().unwrap().join("settings.json.gz");
    let plain = dir.parent().unwrap().join("settings.json");
    export(&dir, &gz, None).unwrap();
    export(&dir, &plain, None).unwrap();
    assert!(std::fs::read(&gz).unwrap().starts_with(&GZIP_MAGIC));
    assert!(std::fs::read_to_string(&plain).unwrap().contains("Apex Legends"), "plain file stays readable");
    assert_eq!(inspect(&gz).unwrap().profiles, 1);
    assert_eq!(inspect(&plain).unwrap().profiles, 1);
    // A small file that unpacks past the limit is refused.
    let bomb = dir.parent().unwrap().join("bomb.json.gz");
    let mut enc = GzEncoder::new(Vec::new(), Compression::best());
    let zeros = vec![b' '; 1024 * 1024];
    for _ in 0..=MAX_IMPORT_BYTES / zeros.len() as u64 {
      enc.write_all(&zeros).unwrap();
    }
    std::fs::write(&bomb, enc.finish().unwrap()).unwrap();
    assert!(inspect(&bomb).unwrap_err().contains("unpacks"));
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
  }

  #[test]
  fn bad_files_are_rejected_before_anything_changes() {
    let ok = r#"{"format":"GamersEqualizer settings","format_version":1}"#;
    assert!(parse(ok).is_ok());
    assert!(parse("{}").is_err());
    assert!(parse(r#"{"format":"Other","format_version":1}"#).is_err());
    assert!(parse(r#"{"format":"GamersEqualizer settings","format_version":99}"#).unwrap_err().contains("newer"));
    let traversal = r#"{"format":"GamersEqualizer settings","format_version":1,"profiles":{"game":{"..\\..\\evil":{"name":"x","vad_type":"game","bands":[],"preamp_db":0,"is_custom":true}}}}"#;
    assert!(parse(traversal).is_err());
    let folder = r#"{"format":"GamersEqualizer settings","format_version":1,"profiles":{"../x":{}}}"#;
    assert!(parse(folder).is_err());
    let unknown = r#"{"format":"GamersEqualizer settings","format_version":1,"settings":{"health":{}}}"#;
    assert!(parse(unknown).is_err());
  }
}
