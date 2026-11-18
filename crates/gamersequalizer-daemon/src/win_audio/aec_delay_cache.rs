//! Small per-device caches in `%APPDATA%\GamersEqualizer`, so the mic chain starts where it left off
//! after a restart instead of re-learning from scratch:
//! - the measured echo delay per (microphone, speaker output) pair,
//! - the voice leveler's learned speech level per microphone (dB x 10),
//! - the measured echo strength per (microphone, speaker output) pair (dB x 10 under the reference).

use log::warn;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "aec_delay_cache.json";
const LEVELER_FILE_NAME: &str = "voice_level_cache.json";
const ECHO_FILE_NAME: &str = "echo_strength_cache.json";

/// `%APPDATA%\GamersEqualizer\aec_delay_cache.json`
pub fn default_path() -> PathBuf {
  app_dir().join(FILE_NAME)
}

/// `%APPDATA%\GamersEqualizer\voice_level_cache.json`, keyed by microphone id.
pub fn leveler_path() -> PathBuf {
  app_dir().join(LEVELER_FILE_NAME)
}

/// `%APPDATA%\GamersEqualizer\echo_strength_cache.json`, keyed like the delay (`key`). The
/// same kind of map as the voice levels: read and written with `load_level` / `save_level`.
pub fn echo_path() -> PathBuf {
  app_dir().join(ECHO_FILE_NAME)
}

/// Per microphone and output. The `|apo` suffix is kept: entries without it were measured against
/// the old WASAPI loopback reference, which differs by about 10 ms, and must not warm-start this one.
pub fn key(capture_id: &str, render_id: &str) -> String {
  format!("{capture_id}|{render_id}|apo")
}

/// Missing or corrupt file -> None (and a log line); never an error for the caller.
pub fn load(path: &Path, key: &str) -> Option<usize> {
  read_map::<u64>(path).get(key).map(|&v| v as usize)
}

/// Atomic write (temp file + rename). Failures are logged and ignored: the cache is an
/// optimisation, the delay is re-measured anyway.
pub fn save(path: &Path, key: &str, delay_samples: usize) {
  update(path, key, delay_samples as u64);
}

pub fn load_level(path: &Path, mic_id: &str) -> Option<i32> {
  read_map::<i32>(path).get(mic_id).copied()
}

pub fn save_level(path: &Path, mic_id: &str, level_db_x10: i32) {
  update(path, mic_id, level_db_x10);
}

/// "Learn again": forget the level of this microphone.
pub fn forget_level(path: &Path, mic_id: &str) {
  let mut map = read_map::<i32>(path);
  if map.remove(mic_id).is_some() {
    write_map(path, &map);
  }
}

fn app_dir() -> PathBuf {
  std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("GamersEqualizer")
}

fn update<T: Serialize + DeserializeOwned>(path: &Path, key: &str, value: T) {
  let mut map = read_map::<T>(path);
  map.insert(key.to_string(), value);
  write_map(path, &map);
}

fn write_map<T: Serialize>(path: &Path, map: &BTreeMap<String, T>) {
  let result = serde_json::to_string_pretty(map)
    .map_err(std::io::Error::other)
    .and_then(|json| crate::output_eq::settings::write_atomic(path, &json));
  if let Err(e) = result {
    warn!("[Cache] could not save {}: {}", path.display(), e);
  }
}

fn read_map<T: DeserializeOwned>(path: &Path) -> BTreeMap<String, T> {
  match fs::read_to_string(path) {
    Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
      warn!("[Cache] {} is corrupt ({}), starting fresh", path.display(), e);
      BTreeMap::new()
    }),
    Err(_) => BTreeMap::new(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_roundtrip_and_corrupt_file() {
    let dir = std::env::temp_dir().join(format!("geq_aec_cache_{}", std::process::id()));
    let path = dir.join(FILE_NAME);
    let _ = fs::remove_file(&path);

    assert_eq!(load(&path, "a|b"), None);
    save(&path, "a|b", 2345);
    save(&path, "c|d", 100);
    assert_eq!(load(&path, "a|b"), Some(2345));
    assert_eq!(load(&path, "c|d"), Some(100));

    let _ = fs::write(&path, "{not json");
    assert_eq!(load(&path, "a|b"), None, "corrupt file must not crash");
    save(&path, "a|b", 7);
    assert_eq!(load(&path, "a|b"), Some(7));
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn key_keeps_the_apo_suffix_of_the_delays_saved_so_far() {
    assert_eq!(key("{mic}", "{out}"), "{mic}|{out}|apo");
  }

  #[test]
  fn voice_level_roundtrip_and_forget() {
    let dir = std::env::temp_dir().join(format!("geq_level_cache_{}", std::process::id()));
    let path = dir.join(LEVELER_FILE_NAME);
    let _ = fs::remove_file(&path);
    save_level(&path, "{mic-1}", -312);
    save_level(&path, "{mic-2}", -250);
    assert_eq!(load_level(&path, "{mic-1}"), Some(-312));
    forget_level(&path, "{mic-1}");
    assert_eq!(load_level(&path, "{mic-1}"), None);
    assert_eq!(load_level(&path, "{mic-2}"), Some(-250));
    let _ = fs::remove_dir_all(&dir);
  }
}
