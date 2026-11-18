//! Optional `%APPDATA%\GamersEqualizer\duck_tuning.json`: Automatic ChatMix's DSP constants,
//! changed by hand while everything runs (a live test can't redeploy the APO mid-firefight). One
//! flat object of name -> number, e.g. `{ "hold_s": 0.6, "release_db_per_s": 10 }`; names and
//! ranges are `DuckTuning::set`'s. `Ducking::tick` looks at it every 2 s and publishes it to the APO.
//!
//! The app never writes this file. Unlike the other state files, a broken one is not moved aside
//! and reset: it is hand-edited and may be caught half-saved, so it is left alone and the previous
//! tuning stays until the file reads again.

use log::{info, warn};
use serde_json::{Map, Value};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use gamersequalizer_common::ducking::DuckTuning;
use gamersequalizer_common::shm::ShmHeader;

const FILE_NAME: &str = "duck_tuning.json";
const CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// What identifies one version of the file: modified time and size; None = no file.
type Stamp = Option<(Option<SystemTime>, u64)>;

pub struct TuningFile {
  path: PathBuf,
  /// The file as last read; None until the first check.
  seen: Option<Stamp>,
  /// The last read failure (warned once per distinct error).
  read_error: Option<String>,
  last_check: Option<Instant>,
  tuning: DuckTuning,
}

impl TuningFile {
  pub fn new() -> Self {
    Self {
      path: crate::win_audio::aec_delay_cache::app_dir().join(FILE_NAME),
      seen: None,
      read_error: None,
      last_check: None,
      tuning: DuckTuning::default(),
    }
  }

  /// The tuning in use.
  pub fn tuning(&self) -> &DuckTuning {
    &self.tuning
  }

  /// Re-reads the file when it changed (looked at every 2 s) and publishes the tuning to `h`.
  pub fn tick(&mut self, h: &ShmHeader) {
    if self.last_check.is_some_and(|t| t.elapsed() < CHECK_INTERVAL) {
      return;
    }
    self.last_check = Some(Instant::now());
    let stamp: Stamp = fs::metadata(&self.path).ok().map(|m| (m.modified().ok(), m.len()));
    if self.seen != Some(stamp) {
      self.reload(stamp);
    }
    // Idempotent (bumps the version only on a change); also covers a freshly created mapping.
    self.tuning.write_to(h);
  }

  fn reload(&mut self, stamp: Stamp) {
    if stamp.is_none() {
      if self.seen.flatten().is_some() {
        info!("[Ducking] tuning: defaults");
      }
      (self.seen, self.read_error, self.tuning) = (Some(stamp), None, DuckTuning::default());
      return;
    }
    let text = match fs::read_to_string(&self.path) {
      Ok(text) => text,
      // Not recorded as seen, so the next check tries again (an editor may hold it while saving).
      Err(e) => {
        let e = e.to_string();
        if self.read_error.as_deref() != Some(e.as_str()) {
          warn!("[Ducking] could not read {FILE_NAME} ({e}); trying again");
          self.read_error = Some(e);
        }
        return;
      }
    };
    (self.seen, self.read_error) = (Some(stamp), None);
    match parse(&text) {
      Ok((tuning, ignored)) => {
        info!("[Ducking] {}", describe(&tuning, &ignored));
        self.tuning = tuning;
      }
      // Logged once per version of the file: it is marked seen.
      Err(e) => warn!("[Ducking] {FILE_NAME} not used ({e}); keeping the previous tuning"),
    }
  }
}

/// The values that differ from the defaults ("hold_s 0.6, ..."), or "defaults".
pub fn tuning_text(tuning: &DuckTuning) -> String {
  let values: Vec<String> = tuning.non_default().iter().map(|(name, v)| format!("{name} {v}")).collect();
  if values.is_empty() { "defaults".to_string() } else { values.join(", ") }
}

impl Default for TuningFile {
  fn default() -> Self {
    Self::new()
  }
}

/// The tuning a file asks for, starting from the defaults, and the names `set` refused (unknown,
/// not a number, out of range).
fn parse(text: &str) -> Result<(DuckTuning, Vec<String>), String> {
  let entries: Map<String, Value> = serde_json::from_str(text).map_err(|e| e.to_string())?;
  let mut tuning = DuckTuning::default();
  let mut ignored = Vec::new();
  for (name, value) in &entries {
    if !value.as_f64().is_some_and(|v| tuning.set(name, v as f32)) {
      ignored.push(name.clone());
    }
  }
  Ok((tuning, ignored))
}

fn describe(tuning: &DuckTuning, ignored: &[String]) -> String {
  let mut line = format!("tuning from {FILE_NAME}: {}", tuning_text(tuning));
  if !ignored.is_empty() {
    line.push_str(&format!("; ignored: {}", ignored.join(", ")));
  }
  line
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn file_entries_apply_and_the_rest_is_named() {
    let (tuning, ignored) = parse(r#"{ "hold_s": 0.6 }"#).unwrap();
    assert!(ignored.is_empty());
    assert_eq!(tuning.non_default(), vec![("hold_s", 0.6)]);
    assert_eq!(describe(&tuning, &ignored), "tuning from duck_tuning.json: hold_s 0.6");

    let (tuning, mut ignored) =
      parse(r#"{ "hold_s": -5, "no_such_knob": 1, "release_db_per_s": "fast", "hold_s_typo": 0.5 }"#).unwrap();
    ignored.sort(); // serde_json's map order depends on its features
    assert!(tuning.non_default().is_empty(), "nothing usable: defaults");
    assert_eq!(ignored, ["hold_s", "hold_s_typo", "no_such_knob", "release_db_per_s"]);
    assert_eq!(
      describe(&tuning, &ignored),
      "tuning from duck_tuning.json: defaults; ignored: hold_s, hold_s_typo, no_such_knob, release_db_per_s"
    );

    assert!(parse(r#"{ "hold_s": 0.6"#).is_err(), "half-saved file");
    assert!(parse("[1, 2]").is_err(), "not an object");
  }
}
