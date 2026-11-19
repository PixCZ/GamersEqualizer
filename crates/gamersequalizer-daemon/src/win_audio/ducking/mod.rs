//! Automatic ChatMix, the service's half. The APO lowers the game's stream while someone in
//! the chat talks (`gamersequalizer_common::ducking`); the service hands it what it cannot know
//! itself and writes the log that explains it:
//! - which streams are game and which chat: the stream router pairs them (`stream_router`);
//! - the channel gains: the mixer publishes them (`ChatMixController::write_duck_gains`);
//! - everything else runs from `Ducking::tick`, called by `apo_link` every 100 ms: the hand-edited
//!   tuning (`tuning_file`), a Game or Chat session volume change (each game stream then learns its
//!   lowering level again, `StreamSlot::duck_volume_epoch`), the reduction the window shows, and the
//!   log: a change-only status line, failures, a game it cannot hear (`unheard`, also shown by the
//!   window), the per-minute summary (`minute_summary`) and, with debug logging on, `debug_log`;
//! - a game the APO cannot hear is lowered by the service through its session volume, from the
//!   output mix (`mix`): `Ducking::tick_mix` at every reference tick, the depth folded into the
//!   mixer's Game sessions by the service loop.
//!
//! Every log line starts with `[Ducking]`; levels and depths are dB with one decimal.

mod debug_log;
mod minute_summary;
mod mix;
mod tuning_file;
mod unheard;
mod view;

pub use mix::{Mode, REFERENCE_MISSING, game_duck_factor, mix_fast, mix_subject, mode, set_session_mode};
pub use unheard::{Evidence, UnheardWatch, playing_game_bands};

use gamersequalizer_common::ducking::DuckSettings;
use gamersequalizer_common::ducking::dbg::FLAG_FAILED;
use gamersequalizer_common::shm::{ShmHeader, ShmLayout};
use gamersequalizer_common::streams::StreamTable;
use log::{debug, info};
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::stream_router;
use debug_log::DebugLog;
use minute_summary::MinuteSummary;
use mix::MixRun;
use tuning_file::TuningFile;
use view::{Settings, StreamView};

/// Spatial sound on the default output (on, format name), from the service's setup check every
/// 5 s (`health`): read there, so no spatial-client or registry call stalls this thread.
static SPATIAL: Mutex<(bool, Option<String>)> = Mutex::new((false, None));

pub struct Ducking {
  tuning: TuningFile,
  /// Slot owners whose failure was logged.
  failed: HashSet<u64>,
  /// The Game and Chat session volumes last seen, and the epoch the game streams were told.
  session_gains: Option<(f32, f32)>,
  volume_epoch: i32,
  /// What the status line said last.
  status: Option<Status>,
  summary: MinuteSummary,
  debug: DebugLog,
  /// A game it cannot hear, and its name as logged (None: not logged).
  unheard: UnheardWatch,
  unheard_logged: Option<String>,
  mix: MixRun,
}

/// The facts of the status line: the reasons the feature may do nothing. A change writes it again.
#[derive(Debug, Clone, PartialEq, Default)]
struct Status {
  enabled: bool,
  /// Who lowers the game (`mode_text`).
  mode: String,
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
      failed: HashSet::new(),
      session_gains: None,
      volume_epoch: 0,
      status: None,
      summary: MinuteSummary::default(),
      debug: DebugLog::default(),
      unheard: UnheardWatch::default(),
      unheard_logged: None,
      mix: MixRun::new(),
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
    let settings = Settings::read(h, self.tuning.tuning());
    let (unheard, name, key) = self.watch_unheard(h, enabled, table, &streams);
    self.mix.set_unheard(unheard && table.is_some(), key);
    self.log_status(enabled, &streams, &settings);
    // Mix mode's run joins the log as one more game stream ("(mix)"); the status line works on the
    // real streams only.
    let mut logged = streams.clone();
    logged.extend(self.mix.view(&name));
    h.duck_reduction_permille.store(view::reduction(&logged), Ordering::Relaxed);
    if !enabled {
      (self.summary, self.debug) = (MinuteSummary::default(), DebugLog::default());
      return;
    }
    if let Some(table) = table {
      // Owners whose slot is gone are forgotten; a new stream gets a new owner value.
      self.failed.retain(|owner| table.slots.iter().any(|s| s.owner.load(Ordering::Acquire) == *owner));
      self.follow_session_gains(table, mix::session_gains());
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
    if let Some(line) = self.summary.tick(now, &logged, &settings) {
      info!("[Ducking] {line}");
    }
    let debug_on = log::log_enabled!(log::Level::Debug);
    for line in self.debug.tick(now, debug_on.then_some((logged.as_slice(), &settings))) {
      debug!("[Ducking] {line}");
    }
  }

  /// Every reference tick (40 ms while a reduction moves, else 50 ms): mix mode's step. `layout`:
  /// the APO's mapping while we drive it.
  pub fn tick_mix(&mut self, h: &ShmHeader, layout: Option<&ShmLayout>) {
    self.mix.step(layout, &DuckSettings::read(h), self.tuning.tuning(), mix::session_gains());
  }

  /// A Game or Chat session volume moved: the stream APOs see it in their levels, but their lowering
  /// level must start over, so every slot gets the next epoch (a stream that comes later starts at
  /// 0 anyway). Mix mode follows the same volumes itself (`MixRun::step`).
  fn follow_session_gains(&mut self, table: &StreamTable, gains: (f32, f32)) {
    if self.session_gains.is_some_and(|was| was != gains) {
      // Positive, so an APO built before (which read a stored level here when negative) ignores it.
      self.volume_epoch = self.volume_epoch % i32::MAX + 1;
      table.slots.iter().for_each(|slot| slot.duck_volume_epoch.store(self.volume_epoch, Ordering::Relaxed));
      debug!("[Ducking] session volumes game {:.2} chat {:.2}: the lowering level starts over", gains.0, gains.1);
    }
    self.session_gains = Some(gains);
  }

  /// Mix mode needs the speaker reference (`aec_reference` asks the output APO for it).
  pub fn wants_reference(&self) -> bool {
    self.mix.wants_reference()
  }

  /// Whether the game cannot be heard at its stream (`unheard`), its name for the log and its store
  /// key; one line when that starts and one when it ends.
  fn watch_unheard(
    &mut self,
    h: &ShmHeader,
    enabled: bool,
    table: Option<&StreamTable>,
    streams: &[StreamView],
  ) -> (bool, String, Option<(String, String)>) {
    let (spatial_on, format) = spatial();
    let bands = match (table.filter(|_| enabled), stream_router::now_hns()) {
      (Some(table), Some(now)) => playing_game_bands(table, now),
      _ => Vec::new(),
    };
    let game = if enabled { h.read_active_game() } else { None };
    let evidence = Evidence { enabled, spatial_on, game_bands: &bands };
    let unheard = self.unheard.observe(Instant::now(), &evidence);
    let game_stream = streams.iter().find(|s| s.is_game);
    let paired = game_stream.map(|s| s.exe.as_str());
    let label = game.as_ref().map(|g| g.label.as_str());
    // The paired game stream is the subject (the rule needs one): its exe (the store key, stable
    // while it plays) and its image path (the sessions the mixer may lower).
    let key = game_stream.map(|s| (s.exe.to_ascii_lowercase(), s.path.clone()));
    let name = paired.or(label).unwrap_or("the game").to_string();
    match (unheard, self.unheard_logged.take()) {
      (true, None) => {
        let format = format.as_ref().map_or(String::new(), |f| format!(" ({f})"));
        info!(
          "[Ducking] {name} cannot be heard: spatial sound{format} is on and the game sends its sound past the \
           per-app effects; nothing to measure or lower at its stream"
        );
        self.unheard_logged = Some(name.clone());
      }
      (false, Some(logged)) => info!("[Ducking] {logged} can be heard again"),
      (_, logged) => self.unheard_logged = logged,
    }
    (unheard, name, key)
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
        mode: mode_text(self.mix.mode(), spatial().1.as_deref()),
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
    format!(
      "on, strength {} %; {}; chat {chat}; game {game}; master switch {master}{debug_lines}",
      strength_permille / 10,
      self.mode
    )
  }
}

/// Who lowers the game, for the status line.
fn mode_text(mode: Mode, spatial: Option<&str>) -> String {
  match mode {
    Mode::Stream => "mode stream".to_string(),
    Mode::Mix => format!("mode mix (spatial sound: {})", spatial.unwrap_or("on")),
    Mode::ChannelOutputs => {
      "mode none: the game plays past the per-app effects, and Game / Chat have their own outputs".to_string()
    }
    Mode::NoReference => "mode none: the game plays past the per-app effects, and the output mix does not reach the \
                          service (no output EQ on that output?)"
      .to_string(),
  }
}

/// The service's setup check (`health`, every 5 s): spatial sound on the default output.
pub fn set_spatial(on: bool, format: Option<String>) {
  if let Ok(mut s) = SPATIAL.lock() {
    *s = (on, format);
  }
}

/// See `SPATIAL`; off if its lock was poisoned.
fn spatial() -> (bool, Option<String>) {
  SPATIAL.lock().map(|s| s.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_session_volume_change_moves_every_slots_epoch_once() {
    let mut mem: Box<std::mem::MaybeUninit<StreamTable>> = Box::new_uninit();
    // SAFETY: all-zero bytes are a valid StreamTable (atomics only), like a fresh mapping.
    let table = unsafe {
      std::ptr::write_bytes(mem.as_mut_ptr(), 0, 1);
      mem.assume_init()
    };
    let epoch = |i: usize| table.slots[i].duck_volume_epoch.load(Ordering::Relaxed);
    let mut ducking = Ducking::new();
    ducking.follow_session_gains(&table, (1.0, 1.0));
    ducking.follow_session_gains(&table, (1.0, 1.0));
    assert_eq!(epoch(0), 0, "the first look and no change move nothing");
    ducking.follow_session_gains(&table, (0.5, 1.0));
    ducking.follow_session_gains(&table, (0.5, 1.0));
    assert_eq!((epoch(0), epoch(3)), (1, 1));
    ducking.follow_session_gains(&table, (0.5, 0.8));
    assert_eq!(epoch(0), 2);
    ducking.volume_epoch = i32::MAX;
    ducking.follow_session_gains(&table, (0.5, 0.7));
    assert_eq!(epoch(0), 1, "wraps to positive");
  }

  #[test]
  fn the_status_line_names_every_reason_to_stay_idle() {
    let status = Status {
      enabled: true,
      mode: mode_text(Mode::Stream, None),
      chat: Some(("Discord.exe".into(), "no (44100 Hz)".into())),
      game: None,
      master_on: false,
      debug_lines: false,
    };
    assert_eq!(
      status.text(500),
      "on, strength 50 %; mode stream; chat Discord.exe, detector no (44100 Hz); game not paired (not recognised as a game, or \
       not playing, or no APO on the output); master switch off: nothing is processed (debug lines off)"
    );
    assert_eq!(Status::default().text(500), "off");
  }
}
