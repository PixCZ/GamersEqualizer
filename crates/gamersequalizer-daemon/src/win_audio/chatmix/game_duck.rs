//! Automatic ChatMix's mix mode lowers the subject game's sessions on the Game channel (the game
//! the service hears only in the mix, `ducking::mix_subject`, matched by exe path). A small state
//! machine the mixer pass drives (`apply_session_mix`, the one writer of session volumes); the COM
//! calls are the mixer's, this decides.
//!
//! Per lowered session: `{ base, written }`, only while we hold it down.
//! 1. Start (`f < 1`): the base is the session's volume now (or what the mixer intends for it); a
//!    muted session or a base at or below 0.001 is not lowered. The state file gets the exe's entry
//!    before the first write, then `base x f` is written.
//! 2. While lowered: a value the mixer intends for the session (its fader, balance, mute) replaces
//!    the base, as if there were no reduction; `base x f` is written on a step of 0.2 dB or more,
//!    and always when the base changed. No reads: a level another program sets meanwhile is
//!    overwritten, and the level from before the reduction comes back (known limit).
//! 3. End (`f` back to 1, the session is no longer the subject's, or it left the Game channel):
//!    exactly the base, always.
//! 4. Losing control (`release`): the base at once for every lowered session.
//! 5. A failed write: one warning for the session, its state dropped, no retry while it lasts; it
//!    does not count as checked (rule 7).
//! 6. `ducking_lowered.json`: one entry per exe path with its base, for crash and exit recovery
//!    only. Added before the first lowering write of the game's mix-mode run, removed when that run
//!    is over with its sessions back at their bases (also at a clean stop). Within a run it keeps
//!    the largest base of that exe's sessions and is written only when that grows; a moved base is
//!    written at most every 5 s. Exe paths are compared ASCII case-insensitively, one rule
//!    everywhere.
//! 7. Recovery: an entry the service started with, or one whose lowered session vanished (the game
//!    exited lowered) or could not be written, waits. Each session of that exe is checked once (by
//!    session id), always before it can be lowered (`begin_pass` runs first): below the entry's
//!    base it is set to it, at or above it is left alone (the user's), and a session the mixer sets
//!    this pass keeps the mixer's value (the fader is the user's will). The entry goes once its
//!    sessions were all checked and none of them is left.
//! 8. Never louder than the base: `f` and bases clamped to 0..=1, a bad depth is no reduction.
//!
//! Known limits: an outside change during a reduction is overwritten; after a crash, a level the
//! user set lower than the stored base before the service came back is raised to the base; one
//! base per exe (after a crash every session of a game is set back to at most its loudest
//! session's level, and only if it is below that); an entry of a game that never runs again stays.

use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::output_eq::settings::{keep_corrupt, write_atomic};

const FILE: &str = "ducking_lowered.json";
/// At or below this a session is silent: nothing to lower.
const SILENT: f32 = 0.001;
/// A recovered session this close under its base is at it (rounding of the stored volume).
const MARGIN: f32 = 0.005;
/// A lowered session moves in steps of at least this (dB): finer ones are not heard but cost a call.
const STEP_DB: f32 = 0.2;
/// A moved base reaches the file at most this often.
const BASE_SAVE_EVERY: Duration = Duration::from_secs(5);

/// One Game session as the mixer pass sees it.
#[derive(Debug, Clone, Copy)]
pub struct Look<'a> {
  /// Session instance id and the app's exe path.
  pub id: &'a str,
  pub path: &'a str,
  /// The volume the mixer sets it to this pass (its own target), if any.
  pub intent: Option<f32>,
}

/// What one session gets this pass, handed back to `done` with the outcome of the write.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Planned {
  pub volume: f32,
  kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
  Lower,
  End,
  Recover,
}

#[derive(Debug, Clone, PartialEq)]
struct Lowered {
  path: String,
  base: f32,
  written: f32,
}

/// The state file: raw entries (unknown fields and unreadable entries survive), keyed by exe path
/// in ASCII lowercase.
#[derive(Debug)]
struct LoweredFile {
  path: PathBuf,
  entries: Map<String, Value>,
  /// False after a read error other than "not found": the file is left as it is.
  writable: bool,
  /// Entries waiting for recovery (rule 7), and those whose present sessions were all checked.
  waiting: HashSet<String>,
  settled: HashSet<String>,
  /// A moved base waits for the file since then.
  base_moved_at: Option<Instant>,
}

#[derive(Debug, Default)]
pub struct GameDuck {
  /// None: the window, which never lowers anything.
  file: Option<LoweredFile>,
  lowered: HashMap<String, Lowered>,
  /// Sessions whose write failed (warned once): left alone while they last.
  failed: HashSet<String>,
  /// Sessions checked for recovery (every session is, once, before it can be lowered).
  checked: HashSet<String>,
}

impl GameDuck {
  /// The service's: what an earlier run left lowered is checked as its sessions appear.
  pub fn for_service(dir: &Path) -> Self {
    let path = dir.join(FILE);
    let (entries, writable) = match std::fs::read_to_string(&path) {
      Ok(text) => match serde_json::from_str::<Map<String, Value>>(&text) {
        Ok(entries) => (entries, true),
        Err(e) => {
          log::warn!("[Ducking] {} is corrupt ({e}), starting without it", path.display());
          keep_corrupt(&path);
          (Map::new(), true)
        }
      },
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Map::new(), true),
      Err(e) => {
        log::warn!("[Ducking] could not read {} ({e}): it is left as it is for this run", path.display());
        (Map::new(), false)
      }
    };
    let waiting: HashSet<String> = entries.keys().cloned().collect();
    if !waiting.is_empty() {
      log::info!(
        "[Ducking] {} game(s) may have been left lowered by an earlier run: checked as they play",
        waiting.len()
      );
    }
    let file = LoweredFile { path, entries, writable, waiting, settled: HashSet::new(), base_moved_at: None };
    Self { file: Some(file), ..Self::default() }
  }

  /// Before a pass: `present` are the Game sessions (id, exe path) now. A lowered session that is
  /// gone leaves its entry waiting (Windows keeps its lowered volume for the app's next start), so
  /// a new session of that exe in this same pass is checked before it can be lowered.
  pub fn begin_pass<'a>(&mut self, present: impl Iterator<Item = (&'a str, &'a str)>) {
    let present: Vec<&str> = present.map(|(id, _)| id).collect();
    let here = |id: &String| present.contains(&id.as_str());
    let gone: Vec<String> = self.lowered.iter().filter(|(id, _)| !here(id)).map(|(_, l)| l.path.clone()).collect();
    self.lowered.retain(|id, _| here(id));
    self.failed.retain(here);
    self.checked.retain(here);
    if let Some(file) = self.file.as_mut() {
      for path in gone {
        file.wait(&path);
      }
    }
  }

  /// One Game session in this pass. `subject`: the mix-mode game's exe path; `factor`: the
  /// reduction (1 = none); `read`: the session's volume and mute now (called at most once). The
  /// volume to write, if any; then `done` with the outcome.
  pub fn session(
    &mut self,
    now: Instant,
    look: Look<'_>,
    subject: Option<&str>,
    factor: f32,
    read: impl FnOnce() -> Option<(f32, bool)>,
  ) -> Option<Planned> {
    let Look { id, path, intent } = look;
    let factor = if factor.is_finite() { factor.clamp(0.0, 1.0) } else { 1.0 };
    let intent = intent.map(clamp01);
    let mut read = Some(read);
    let mut current = move || read.take().and_then(|r| r());
    // Rule 7, before anything else. A failed read is checked again next pass; a session the mixer
    // sets this pass keeps the mixer's value.
    if !self.lowered.contains_key(id) && !self.checked.contains(id) && !self.failed.contains(id) {
      let waiting = self.file.as_ref().and_then(|f| f.waiting_base(path)).filter(|_| intent.is_none());
      if let Some(base) = waiting {
        let (volume, _) = current()?;
        self.checked.insert(id.to_string());
        if volume < base - MARGIN {
          return Some(Planned { volume: base, kind: Kind::Recover });
        }
      } else {
        self.checked.insert(id.to_string());
      }
    }
    let lowering = factor < 1.0 && subject.is_some_and(|s| same(s, path));
    if let Some(l) = self.lowered.get_mut(id) {
      let moved = intent.is_some_and(|i| i != l.base);
      if let Some(i) = intent {
        l.base = i;
      }
      let (base, written) = (l.base, l.written);
      if moved {
        self.store_largest_base(now, path);
      }
      if !lowering {
        return Some(Planned { volume: base, kind: Kind::End });
      }
      let target = base * factor;
      return (moved || step_due(written, target)).then_some(Planned { volume: target, kind: Kind::Lower });
    }
    if !lowering || self.failed.contains(id) {
      return None;
    }
    // Rule 1: a reduction starts, relative to the volume the session has now.
    let (volume, muted) = current()?;
    let base = intent.unwrap_or(clamp01(volume));
    if muted || base <= SILENT {
      return None;
    }
    if let Some(file) = self.file.as_mut() {
      file.add(path, base);
    }
    self.lowered.insert(id.to_string(), Lowered { path: path.to_string(), base, written: volume });
    Some(Planned { volume: base * factor, kind: Kind::Lower })
  }

  /// A lowered session left the Game channel (dragged to another one): its base, to write before
  /// it is let go (then `done`).
  pub fn left_game(&self, id: &str) -> Option<(String, Planned)> {
    self.lowered.get(id).map(|l| (l.path.clone(), Planned { volume: l.base, kind: Kind::End }))
  }

  /// The outcome of writing `planned` to the session.
  pub fn done(&mut self, look: Look<'_>, planned: Planned, ok: bool) {
    let Look { id, path, .. } = look;
    if !ok {
      // Rule 5: warned once, state dropped, not checked; the entry waits for a later session.
      if self.failed.insert(id.to_string()) {
        log::warn!("[Ducking] {path}: could not set the volume; this session is left alone");
      }
      self.checked.remove(id);
      let was_lowered = self.lowered.remove(id).is_some();
      if let Some(file) = self.file.as_mut()
        && (was_lowered || planned.kind == Kind::Recover)
      {
        file.wait(path);
      }
      return;
    }
    match planned.kind {
      Kind::Lower => {
        if let Some(l) = self.lowered.get_mut(id) {
          l.written = planned.volume;
        }
      }
      Kind::End => {
        self.lowered.remove(id);
      }
      Kind::Recover => {
        log::info!(
          "[Ducking] {path}: volume set back to {:.0} % after an earlier run left it lowered",
          planned.volume * 100.0
        );
      }
    }
  }

  /// Rule 4: the mixer loses the session volumes. Every lowered session (id, path) with its base,
  /// to write at once (then `done`, then `end_pass` with no subject).
  pub fn release(&self) -> Vec<(String, String, Planned)> {
    let end = |l: &Lowered| Planned { volume: l.base, kind: Kind::End };
    self.lowered.iter().map(|(id, l)| (id.clone(), l.path.clone(), end(l))).collect()
  }

  /// After every pass: entries that are done go, and a moved base is written when due.
  pub fn end_pass<'a>(
    &mut self,
    now: Instant,
    subject: Option<&str>,
    present: impl Iterator<Item = (&'a str, &'a str)>,
  ) {
    let present: Vec<(&str, &str)> = present.collect();
    let Some(file) = self.file.as_mut() else { return };
    for key in file.readable_keys() {
      let active = subject.is_some_and(|s| same(s, &key)) || self.lowered.values().any(|l| same(&l.path, &key));
      let mine: Vec<&str> = present.iter().filter(|(_, p)| same(p, &key)).map(|(id, _)| *id).collect();
      let all_checked = mine.iter().all(|id| self.checked.contains(*id));
      if active {
        file.settled.remove(&key);
      } else if !file.waiting.contains(&key) {
        // This run's: over, and its sessions have their bases back.
        if all_checked {
          file.remove(&key);
        }
      } else {
        if !mine.is_empty() && all_checked {
          file.settled.insert(key.clone());
        }
        if mine.is_empty() && file.settled.contains(&key) {
          file.remove(&key);
        }
      }
    }
    file.save_moved_base(now);
  }

  pub fn is_lowered(&self, id: &str) -> bool {
    self.lowered.contains_key(id)
  }

  /// A base moved: the file keeps the largest base among this exe's lowered sessions.
  fn store_largest_base(&mut self, now: Instant, path: &str) {
    let largest = self.lowered.values().filter(|l| same(&l.path, path)).map(|l| l.base).fold(0.0f32, f32::max);
    if let Some(file) = self.file.as_mut() {
      file.set_base(now, path, largest);
    }
  }
}

impl LoweredFile {
  fn key(path: &str) -> String {
    path.to_ascii_lowercase()
  }

  /// The entry's base, when it is readable.
  fn base(&self, path: &str) -> Option<f32> {
    let base = self.entries.get(&Self::key(path))?.get("base")?.as_f64()? as f32;
    (base.is_finite() && (0.0..=1.0).contains(&base)).then_some(base)
  }

  /// The base of an entry that waits for recovery.
  fn waiting_base(&self, path: &str) -> Option<f32> {
    if self.waiting.contains(&Self::key(path)) { self.base(path) } else { None }
  }

  /// Keys of the readable entries.
  fn readable_keys(&self) -> Vec<String> {
    self.entries.keys().filter(|k| self.base(k).is_some()).cloned().collect()
  }

  /// Before a lowering write: the entry is on disk. A new run puts its base; within a run the
  /// entry only grows (two sessions of one exe: the louder one's level).
  fn add(&mut self, path: &str, base: f32) {
    let key = Self::key(path);
    let new_run = self.waiting.remove(&key) | self.settled.remove(&key);
    if !new_run && self.base(path).is_some_and(|stored| stored >= base) {
      return;
    }
    self.put(path, base);
    self.save();
  }

  /// A lowered session vanished or could not be written: its entry waits for recovery.
  fn wait(&mut self, path: &str) {
    if self.base(path).is_some() {
      self.waiting.insert(Self::key(path));
      self.settled.remove(&Self::key(path));
    }
  }

  /// A base that moved while lowered: on disk within `BASE_SAVE_EVERY`.
  fn set_base(&mut self, now: Instant, path: &str, base: f32) {
    if self.base(path) != Some(base) {
      self.put(path, base);
      self.base_moved_at.get_or_insert(now);
    }
  }

  fn remove(&mut self, key: &str) {
    self.waiting.remove(key);
    self.settled.remove(key);
    if self.entries.remove(key).is_some() {
      self.save();
    }
  }

  fn save_moved_base(&mut self, now: Instant) {
    if self.base_moved_at.is_some_and(|t| now.saturating_duration_since(t) >= BASE_SAVE_EVERY) {
      self.save();
    }
  }

  /// Keeps the entry's other fields.
  fn put(&mut self, path: &str, base: f32) {
    let entry = self.entries.entry(Self::key(path)).or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
      *entry = Value::Object(Map::new());
    }
    if let Value::Object(fields) = entry {
      fields.insert("base".into(), Value::from(f64::from(base)));
    }
  }

  fn save(&mut self) {
    self.base_moved_at = None;
    if !self.writable {
      return;
    }
    let result = if self.entries.is_empty() {
      match std::fs::remove_file(&self.path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
      }
    } else {
      serde_json::to_string_pretty(&self.entries)
        .map_err(std::io::Error::other)
        .and_then(|json| write_atomic(&self.path, &json))
    };
    if let Err(e) = result {
      log::warn!("[Ducking] could not update {}: {e}", self.path.display());
    }
  }
}

/// The same exe path: the one comparison used everywhere (ASCII case-insensitive, other characters
/// as they are; `LoweredFile::key` lowercases the same way).
fn same(a: &str, b: &str) -> bool {
  a.eq_ignore_ascii_case(b)
}

fn clamp01(v: f32) -> f32 {
  if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 }
}

/// A move worth a call: `STEP_DB` or more.
fn step_due(written: f32, target: f32) -> bool {
  let db = |v: f32| 20.0 * v.max(1e-6).log10();
  (db(target) - db(written)).abs() >= STEP_DB
}

#[cfg(test)]
mod tests;
