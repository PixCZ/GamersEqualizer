//! Learned levels per game, so a game's second run starts where the first left off instead of
//! learning its fights again: `%APPDATA%\GamersEqualizer\duck_level_cache.json`,
//! `{ "game.exe": { "loud_db_x10": -215, "quiet_db_x10": -290 } }`, keyed by the exe in lowercase
//! (as `app_channels` matches it). The APO publishes a level only once it is settled, so only
//! established values get here. Read and written by `Ducking::tick` only.
//!
//! Missing or corrupt file: start empty (a corrupt one is kept aside). Entries and fields this build
//! cannot read (a newer version's) are written back as they were.

use log::{info, warn};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::output_eq::settings::{keep_corrupt, write_atomic};

const FILE_NAME: &str = "duck_level_cache.json";
/// Plausible speech-band levels (dB x 10); anything else stored is ignored, never handed to the APO.
const LEVEL_RANGE: RangeInclusive<i32> = -1000..=-1;
/// A record is updated when a level moved by 1 dB or more.
const MIN_CHANGE_X10: i32 = 10;
/// A game's learned levels are logged again only when one moved this far (3 dB) since the last line.
const LOG_CHANGE_X10: i32 = 30;
const SAVE_INTERVAL: Duration = Duration::from_secs(30);

/// (loud level of the game, quiet talker: the quietest of the people who normally talk), dB x 10 of
/// the raw speech band; 0 = not learned.
pub type Levels = (i32, i32);

#[derive(Default, Serialize, Deserialize)]
struct Record {
  #[serde(default)]
  loud_db_x10: i32,
  #[serde(default)]
  quiet_db_x10: i32,
  /// Fields a newer version added.
  #[serde(flatten)]
  extra: Map<String, Value>,
}

pub struct LevelStore {
  path: PathBuf,
  /// Every entry as read, so the ones this build cannot use survive a save.
  raw: Map<String, Value>,
  levels: HashMap<String, Levels>,
  /// Games whose record changed since the last write.
  changed: BTreeSet<String>,
  last_save: Option<Instant>,
  /// Levels last logged per game in this service run.
  logged: HashMap<String, Levels>,
}

impl LevelStore {
  pub fn load_default() -> Self {
    Self::load(crate::win_audio::aec_delay_cache::app_dir().join(FILE_NAME))
  }

  pub fn load(path: PathBuf) -> Self {
    let raw = match fs::read_to_string(&path) {
      Ok(text) => serde_json::from_str::<Map<String, Value>>(&text).unwrap_or_else(|e| {
        warn!("[Ducking] {} is corrupt ({e}), starting empty", path.display());
        keep_corrupt(&path);
        Map::new()
      }),
      Err(_) => Map::new(),
    };
    let levels = raw
      .iter()
      .filter_map(|(exe, value)| {
        let record = serde_json::from_value::<Record>(value.clone()).ok()?;
        let levels = (valid(record.loud_db_x10), valid(record.quiet_db_x10));
        (levels != (0, 0)).then(|| (exe.clone(), levels))
      })
      .collect();
    Self { path, raw, levels, changed: BTreeSet::new(), last_save: None, logged: HashMap::new() }
  }

  /// What an earlier run learned for this game (`exe` lowercase).
  pub fn get(&self, exe: &str) -> Option<Levels> {
    self.levels.get(exe).copied()
  }

  /// Takes what the game's loudest stream reports now.
  pub fn learn(&mut self, exe: &str, reported: Levels) {
    if let Some(new) = updated(self.get(exe), reported) {
      self.levels.insert(exe.to_string(), new);
      self.changed.insert(exe.to_string());
    }
  }

  /// Writes the file when a record changed, at most every `SAVE_INTERVAL`.
  pub fn save_if_due(&mut self) {
    if self.changed.is_empty() || self.last_save.is_some_and(|t| t.elapsed() < SAVE_INTERVAL) {
      return;
    }
    self.last_save = Some(Instant::now());
    for exe in std::mem::take(&mut self.changed) {
      let Some(&(loud, quiet)) = self.levels.get(&exe) else { continue };
      let mut record =
        self.raw.get(&exe).and_then(|v| serde_json::from_value::<Record>(v.clone()).ok()).unwrap_or_default();
      (record.loud_db_x10, record.quiet_db_x10) = (loud, quiet);
      match serde_json::to_value(&record) {
        Ok(value) => {
          self.raw.insert(exe.clone(), value);
          if self
            .logged
            .get(&exe)
            .is_none_or(|&(l, q)| moved(l, loud, LOG_CHANGE_X10) || moved(q, quiet, LOG_CHANGE_X10))
          {
            info!("[Ducking] learned for {exe}: {}", super::levels_text((loud, quiet)));
            self.logged.insert(exe.clone(), (loud, quiet));
          }
        }
        Err(e) => warn!("[Ducking] could not store the levels of {exe}: {e}"),
      }
    }
    let result = serde_json::to_string_pretty(&self.raw)
      .map_err(std::io::Error::other)
      .and_then(|json| write_atomic(&self.path, &json));
    if let Err(e) = result {
      warn!("[Ducking] could not save {}: {e}", self.path.display());
    }
  }
}

/// Per game, the report of its stream with the highest loud level (a game may play several streams).
/// Reports with nothing learned are left out.
pub fn loudest_per_game<'a>(reports: impl IntoIterator<Item = (&'a str, Levels)>) -> HashMap<&'a str, Levels> {
  // A missing loud level ranks below any real one.
  let rank = |l: Levels| if valid(l.0) == 0 { i32::MIN } else { l.0 };
  let mut best: HashMap<&str, Levels> = HashMap::new();
  for (exe, levels) in reports.into_iter().filter(|(_, l)| (valid(l.0), valid(l.1)) != (0, 0)) {
    let current = best.entry(exe).or_insert(levels);
    if rank(levels) > rank(*current) {
      *current = levels;
    }
  }
  best
}

/// The record after a stream reported `reported`, or None when neither level moved by 1 dB (or a
/// value appeared). A reported value that is out of range keeps the stored one.
fn updated(old: Option<Levels>, reported: Levels) -> Option<Levels> {
  let (old_loud, old_quiet) = old.unwrap_or((0, 0));
  let take = |old: i32, new: i32| if valid(new) != 0 { new } else { old };
  let new = (take(old_loud, reported.0), take(old_quiet, reported.1));
  (moved(old_loud, new.0, MIN_CHANGE_X10) || moved(old_quiet, new.1, MIN_CHANGE_X10)).then_some(new)
}

/// A level moved by `by` or more, or appeared / went (0 = none).
fn moved(a: i32, b: i32, by: i32) -> bool {
  (a - b).abs() >= by || (a == 0) != (b == 0)
}

/// The value if it is a plausible level, else 0 (none).
pub(super) fn valid(level_db_x10: i32) -> i32 {
  if LEVEL_RANGE.contains(&level_db_x10) { level_db_x10 } else { 0 }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("geq_level_store_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.join(FILE_NAME)
  }

  /// Saves now, whatever the interval.
  fn flush(cache: &mut LevelStore) {
    cache.last_save = None;
    cache.save_if_due();
  }

  #[test]
  fn learned_levels_survive_a_restart() {
    let path = temp_path("roundtrip");
    let mut cache = LevelStore::load(path.clone());
    assert_eq!(cache.get("game.exe"), None);
    cache.learn("game.exe", (-215, -290));
    flush(&mut cache);
    assert_eq!(LevelStore::load(path.clone()).get("game.exe"), Some((-215, -290)));
    let _ = fs::remove_dir_all(path.parent().unwrap());
  }

  #[test]
  fn corrupt_file_starts_empty_and_is_kept_aside() {
    let path = temp_path("corrupt");
    fs::write(&path, "{ nope").unwrap();
    let mut cache = LevelStore::load(path.clone());
    assert_eq!(cache.get("game.exe"), None);
    assert!(path.with_file_name(format!("{FILE_NAME}.corrupt")).exists());
    cache.learn("game.exe", (-200, -300));
    flush(&mut cache);
    assert_eq!(LevelStore::load(path.clone()).get("game.exe"), Some((-200, -300)));
    let _ = fs::remove_dir_all(path.parent().unwrap());
  }

  #[test]
  fn a_newer_versions_fields_and_entries_survive_a_save() {
    let path = temp_path("newer");
    fs::write(
      &path,
      r#"{ "game.exe": { "loud_db_x10": -200, "quiet_db_x10": -300, "curve": [1, 2] },
           "other.exe": "a shape this build does not know" }"#,
    )
    .unwrap();
    let mut cache = LevelStore::load(path.clone());
    assert_eq!(cache.get("other.exe"), None);
    cache.learn("game.exe", (-150, -300));
    flush(&mut cache);
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(r#""curve""#) && text.contains("a shape this build does not know"), "{text}");
    assert_eq!(LevelStore::load(path.clone()).get("game.exe"), Some((-150, -300)));
    let _ = fs::remove_dir_all(path.parent().unwrap());
  }

  #[test]
  fn implausible_stored_levels_are_dropped() {
    let path = temp_path("range");
    fs::write(
      &path,
      r#"{ "a.exe": { "loud_db_x10": 50, "quiet_db_x10": -5000 },
           "b.exe": { "loud_db_x10": -200, "quiet_db_x10": 7 } }"#,
    )
    .unwrap();
    let cache = LevelStore::load(path.clone());
    assert_eq!(cache.get("a.exe"), None);
    assert_eq!(cache.get("b.exe"), Some((-200, 0)));
    let _ = fs::remove_dir_all(path.parent().unwrap());
  }

  #[test]
  fn a_record_moves_by_one_db_and_the_loudest_stream_wins() {
    assert_eq!(updated(None, (-200, -300)), Some((-200, -300)));
    assert_eq!(updated(Some((-200, -300)), (-195, -305)), None, "under 1 dB");
    assert_eq!(updated(Some((-200, -300)), (-190, -300)), Some((-190, -300)));
    assert_eq!(updated(Some((-200, -300)), (0, -280)), Some((-200, -280)), "nothing reported keeps the old");
    assert_eq!(updated(Some((-200, -300)), (2000, 0)), None, "implausible reports change nothing");

    let best =
      loudest_per_game([("g.exe", (-250, -300)), ("g.exe", (-180, -310)), ("g.exe", (0, -290)), ("h.exe", (0, 0))]);
    assert_eq!(best.get("g.exe"), Some(&(-180, -310)));
    assert_eq!(best.get("h.exe"), None);
  }
}
