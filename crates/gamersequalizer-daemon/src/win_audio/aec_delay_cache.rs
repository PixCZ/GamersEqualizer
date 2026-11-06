//! Remembers the measured echo delay per (microphone, speaker output) pair, so the echo
//! canceller starts aligned after a restart instead of re-measuring from scratch.

use log::warn;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "aec_delay_cache.json";

/// `%APPDATA%\GamersEqualizer\aec_delay_cache.json`
pub fn default_path() -> PathBuf {
  std::env::var_os("APPDATA")
    .map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from("."))
    .join("GamersEqualizer")
    .join(FILE_NAME)
}

pub fn key(capture_id: &str, render_id: &str) -> String {
  format!("{capture_id}|{render_id}")
}

/// Missing or corrupt file -> None (and a log line); never an error for the caller.
pub fn load(path: &Path, key: &str) -> Option<usize> {
  read_map(path).get(key).map(|&v| v as usize)
}

/// Atomic write (temp file + rename). Failures are logged and ignored: the cache is an
/// optimisation, the delay is re-measured anyway.
pub fn save(path: &Path, key: &str, delay_samples: usize) {
  let mut map = read_map(path);
  map.insert(key.to_string(), delay_samples as u64);
  let result = (|| -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
      fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(&map).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)
  })();
  if let Err(e) = result {
    warn!("[AEC] could not save delay cache {}: {}", path.display(), e);
  }
}

fn read_map(path: &Path) -> BTreeMap<String, u64> {
  match fs::read_to_string(path) {
    Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
      warn!("[AEC] delay cache {} is corrupt ({}), starting fresh", path.display(), e);
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
}
