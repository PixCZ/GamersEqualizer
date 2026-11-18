//! Automatic ChatMix, the service's half. The APO lowers the game's speech band while someone in
//! the chat talks (`gamersequalizer_common::ducking`); the service hands it what it cannot know
//! itself and writes the log that explains it:
//! - which streams are game and which chat: the stream router pairs them (`stream_router`);
//! - the channel gains: the mixer publishes them (`ChatMixController::write_duck_gains`);
//! - everything else runs from `Ducking::tick`, called by `apo_link` every 100 ms: the hand-edited
//!   tuning (`tuning_file`), the levels each game learned in earlier runs (`level_store`), the
//!   reduction the window shows, and the log: a change-only status line, failures, the per-minute
//!   summary (`minute_summary`) and, with debug logging on, `debug_log`.
//!
//! Every log line starts with `[Ducking]`; levels and depths are dB with one decimal.

mod debug_log;
mod level_store;
mod minute_summary;
mod tuning_file;
mod view;

use gamersequalizer_common::ducking::dbg::FLAG_FAILED;
use gamersequalizer_common::shm::ShmHeader;
use gamersequalizer_common::streams::StreamTable;
use log::{debug, info};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::stream_router;
use debug_log::DebugLog;
use level_store::LevelStore;
use minute_summary::MinuteSummary;
use tuning_file::TuningFile;
use view::{Settings, StreamView};

pub struct Ducking {
  tuning: TuningFile,
  /// Loaded on first use.
  store: Option<LevelStore>,
  /// Slot owners already handed the stored levels (or that found none), and whose failure was logged.
  handed: HashSet<u64>,
  failed: HashSet<u64>,
  /// What the status line said last.
  status: Option<Status>,
  summary: MinuteSummary,
  debug: DebugLog,
}

/// The facts of the status line: the reasons the feature may do nothing. A change writes it again.
#[derive(Debug, Clone, PartialEq, Default)]
struct Status {
  enabled: bool,
  /// The chat stream's exe and detector ("yes", "no (44100 Hz)", "?").
  chat: Option<(String, String)>,
  game: Option<String>,
  master_on: bool,
  debug_lines: bool,
}

impl Ducking {
  pub fn new() -> Self {
    Self {
      tuning: TuningFile::new(),
      store: None,
      handed: HashSet::new(),
      failed: HashSet::new(),
      status: None,
      summary: MinuteSummary::default(),
      debug: DebugLog::default(),
    }
  }

  /// Every 100 ms. `h`: the service's mapping (settings in, the reduction out; mirrored into the
  /// APO's by `apo_link`). `table`: the APO's stream table, None while we don't drive the APO.
  pub fn tick(&mut self, h: &ShmHeader, table: Option<&StreamTable>) {
    self.tuning.tick(h);
    let enabled = h.duck_enabled.load(Ordering::Relaxed);
    let streams = match (table.filter(|_| enabled), stream_router::now_hns()) {
      (Some(table), Some(now)) => view::read_streams(table, stream_router::paired_streams(), now),
      _ => Vec::new(),
    };
    h.duck_reduction_permille.store(view::reduction(&streams), Ordering::Relaxed);
    let settings = Settings::read(h, self.tuning.tuning());
    self.log_status(enabled, &streams, &settings);
    if !enabled {
      (self.summary, self.debug) = (MinuteSummary::default(), DebugLog::default());
      return;
    }
    if let Some(table) = table {
      // Owners whose slot is gone are forgotten; a new stream gets a new owner value.
      let live = |owner: &u64| table.slots.iter().any(|s| s.owner.load(Ordering::Acquire) == *owner);
      self.handed.retain(live);
      self.failed.retain(live);
      let store = self.store.get_or_insert_with(LevelStore::load_default);
      hand_and_learn(store, &mut self.handed, table, &streams);
    }
    for s in streams.iter().filter(|s| view::flag(&s.dbg, FLAG_FAILED)) {
      if self.failed.insert(s.owner) {
        info!(
          "[Ducking] stream #{} of {}: processing failed in the audio engine, it passes audio through",
          s.owner as u32, s.exe
        );
      }
    }
    let now = Instant::now();
    if let Some(line) = self.summary.tick(now, &streams, &settings) {
      info!("[Ducking] {line}");
    }
    let debug_on = log::log_enabled!(log::Level::Debug);
    for line in self.debug.tick(now, debug_on.then_some((streams.as_slice(), &settings))) {
      debug!("[Ducking] {line}");
    }
  }

  fn log_status(&mut self, enabled: bool, streams: &[StreamView], settings: &Settings) {
    let status = if enabled {
      Status {
        enabled,
        chat: streams.iter().find(|s| !s.is_game).map(|s| {
          let detector = match view::detector(s) {
            "no" => format!("no ({} Hz)", s.sample_rate),
            other => other.to_string(),
          };
          (s.exe.clone(), detector)
        }),
        game: streams.iter().find(|s| s.is_game).map(|s| s.exe.clone()),
        master_on: settings.master_on,
        debug_lines: crate::app_log::debug_to_disk(),
      }
    } else {
      Status::default()
    };
    if self.status.as_ref() == Some(&status) {
      return;
    }
    info!("[Ducking] {}", status.text(settings.strength_permille));
    self.status = Some(status);
  }
}

impl Default for Ducking {
  fn default() -> Self {
    Self::new()
  }
}

impl Status {
  /// The strength is in the line but not in the facts: a slider drag would write a line per tick.
  fn text(&self, strength_permille: u32) -> String {
    if !self.enabled {
      return "off".to_string();
    }
    let chat = self.chat.as_ref().map_or_else(
      || "not paired (not on the Chat channel, on another output device, or silent so far)".to_string(),
      |(exe, detector)| format!("{exe}, detector {detector}"),
    );
    let game = self
      .game
      .clone()
      .unwrap_or_else(|| "not paired (not recognised as a game, or not playing, or no APO on the output)".to_string());
    let master = if self.master_on { "on" } else { "off: nothing is processed" };
    let debug_lines = if self.debug_lines { "" } else { " (debug lines off)" };
    format!("on, strength {} %; chat {chat}; game {game}; master switch {master}{debug_lines}", strength_permille / 10)
  }
}

/// Hands each new game stream what an earlier run of its game learned, once, at its first tick as
/// a game stream: later the store holds this run's own values, which the APO would take as
/// established. Then keeps what the loudest playing stream of each game has learned.
fn hand_and_learn(store: &mut LevelStore, handed: &mut HashSet<u64>, table: &StreamTable, streams: &[StreamView]) {
  let games: Vec<(&StreamView, String)> =
    streams.iter().filter(|s| s.is_game).map(|s| (s, s.exe.to_ascii_lowercase())).collect();
  for (s, exe) in &games {
    if !handed.insert(s.owner) {
      continue;
    }
    let slot = table.slots.get(s.index).filter(|slot| slot.owner.load(Ordering::Acquire) == s.owner);
    let (Some(slot), Some(levels)) = (slot, store.get(exe)) else { continue };
    slot.duck_saved_loud_db_x10.store(levels.0, Ordering::Relaxed);
    slot.duck_saved_quiet_db_x10.store(levels.1, Ordering::Relaxed);
    info!("[Ducking] stream #{} of {exe} was handed stored levels: {}", s.owner as u32, levels_text(levels));
  }
  let reports = games.iter().filter(|(s, _)| s.fresh).map(|(s, exe)| (exe.as_str(), s.learned));
  for (exe, learned) in level_store::loudest_per_game(reports) {
    store.learn(exe, learned);
  }
  store.save_if_due();
}

/// "-20.5 dB", or "none" for 0 (not learned).
fn level_text(x10: i32) -> String {
  if x10 == 0 { "none".to_string() } else { format!("{:.1} dB", x10 as f32 / 10.0) }
}

/// "loud level -20.5 dB, quiet talker -33.0 dB"
fn levels_text((loud, quiet): (i32, i32)) -> String {
  format!("loud level {}, quiet talker {}", level_text(loud), level_text(quiet))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn zeroed_table() -> Box<StreamTable> {
    let mut mem: Box<std::mem::MaybeUninit<StreamTable>> = Box::new_uninit();
    // SAFETY: all-zero bytes are a valid StreamTable (atomics only), like a fresh mapping.
    unsafe {
      std::ptr::write_bytes(mem.as_mut_ptr(), 0, 1);
      mem.assume_init()
    }
  }

  /// A playing game stream in slot `index` that reports `learned`.
  fn game(table: &StreamTable, index: usize, owner: u64, learned: (i32, i32)) -> StreamView {
    table.slots[index].owner.store(owner, Ordering::Relaxed);
    StreamView { index, learned, ..view::tests::stream(owner, true) }
  }

  #[test]
  fn a_game_stream_is_handed_stored_levels_once_and_never_its_own() {
    let dir = std::env::temp_dir().join(format!("geq_ducking_hand_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut store = LevelStore::load(dir.join("duck_level_cache.json"));
    let table = zeroed_table();
    let mut handed = HashSet::new();
    let saved = |i: usize| {
      let s = &table.slots[i];
      (s.duck_saved_loud_db_x10.load(Ordering::Relaxed), s.duck_saved_quiet_db_x10.load(Ordering::Relaxed))
    };

    // First run, empty store: the stream's own levels flow back, never into its slot.
    let first = [game(&table, 0, (7 << 32) | 1, (-200, -300))];
    for _ in 0..3 {
      hand_and_learn(&mut store, &mut handed, &table, &first);
    }
    assert_eq!(store.get("game.exe"), Some((-200, -300)));
    assert_eq!(saved(0), (0, 0));

    // Second run: a new stream starts from the record, written once and not after the store moved.
    let second = [game(&table, 1, (7 << 32) | 2, (0, 0))];
    hand_and_learn(&mut store, &mut handed, &table, &second);
    assert_eq!(saved(1), (-200, -300));
    store.learn("game.exe", (-100, -250));
    hand_and_learn(&mut store, &mut handed, &table, &second);
    assert_eq!(saved(1), (-200, -300));
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn the_status_line_names_every_reason_to_stay_idle() {
    let status = Status {
      enabled: true,
      chat: Some(("Discord.exe".into(), "no (44100 Hz)".into())),
      game: None,
      master_on: false,
      debug_lines: false,
    };
    assert_eq!(
      status.text(500),
      "on, strength 50 %; chat Discord.exe, detector no (44100 Hz); game not paired (not recognised as a game, or \
       not playing, or no APO on the output); master switch off: nothing is processed (debug lines off)"
    );
    assert_eq!(Status::default().text(500), "off");
  }
}
