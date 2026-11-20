//! The debug lines (`[Ducking]` at debug level, like `[Chain]`): a start line with the settings
//! and the paired streams, then per 10 s window one line per game stream with what the ducker saw
//! and decided (`StreamSlot::duck_dbg`, written by the APO every block). One real call must leave
//! everything tuning needs in the log. Fed every 100 ms by `Ducking::tick`, it samples every tenth
//! 10 ms block: min/max are of those samples, not of every block.
//!
//! Written only when something happened (disk wear: no heartbeat). A window's line is kept when
//! someone talked (game stream talking or chat speech), the chat carried sound the detector did not
//! take as speech, the game was lowered, the lowering level moved by 1 dB or more or was raised or
//! started over since the last line written for that stream, or it is the stream's first window
//! after its start line (one baseline per session). Otherwise the window is dropped: an idle evening
//! writes nothing.

use gamersequalizer_common::ducking::dbg::{
  self, BAND_DB, DEPTH_DB, FLAG_LOWERING, FLAG_TALKING, FLAGS, GAME_DB, LEVEL_DB, LEVEL_RAISES, NEED_DB, VAD_PROB,
};
use gamersequalizer_common::streams::DUCK_DBG_SLOTS;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::view::{self, Settings, StreamView};

const SUMMARY_EVERY: Duration = Duration::from_secs(10);
/// The lowering level moving this much (dB x 10) since the last written line keeps a window.
const LEVEL_MOVE_X10: u32 = 10;

#[derive(Debug, Default)]
pub struct DebugLog {
  /// (owner, is game, detector) of the paired streams at the last start line; None = not started.
  paired: Option<Vec<(u64, bool, &'static str)>>,
  /// (strength, game gain, chat gain) last written.
  settings: Option<(u32, u32, u32)>,
  since: Option<Instant>,
  chat: ChatStats,
  games: BTreeMap<u64, GameStats>,
  /// Per game stream: its lowering level as of the last line written for it. A stream missing
  /// here gets its baseline line.
  written: BTreeMap<u64, Level>,
  /// A start line was written and no window closed since: the chat-only line is a baseline too.
  baseline: bool,
}

/// A game stream's lowering level (dB x 10) and how often it was raised, compared between written
/// lines. Both are the other process's raw values (`dbg::NA` when missing).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Level {
  level: i32,
  raises: i32,
}

/// min/avg/max of dB x 10 samples; `dbg::NA` samples are skipped.
#[derive(Debug, Default, Clone, Copy)]
struct Stat {
  min: i32,
  max: i32,
  sum: i64,
  n: u32,
}

#[derive(Debug, Default)]
struct ChatStats {
  band: Stat,
  speech: Stat,
  samples: u32,
  speech_samples: u32,
  /// Samples whose band level is above the talk threshold (speech or not).
  sound_samples: u32,
  prob_max: Option<i32>,
  detector: bool,
}

#[derive(Debug, Default)]
struct GameStats {
  exe: String,
  band: Stat,
  need: Stat,
  depth: Stat,
  samples: u32,
  talking: u32,
  lowering: u32,
  spurts: u32,
  lowered_spurts: u32,
  /// Some sample had a depth above 0.
  lowered: bool,
  prev_flags: i32,
  /// The last sample's values.
  last: Option<[i32; DUCK_DBG_SLOTS]>,
  /// Mix mode only: the mix level and the chat band level it used.
  mix_level: Stat,
  chat_band: Stat,
  /// Mix mode samples in which the chat made sound (the game's level held).
  chat_sounding: u32,
}

impl DebugLog {
  /// One tick; `view` None = debug logging is off (starts over). The lines to log.
  pub fn tick(&mut self, now: Instant, view: Option<(&[StreamView], &Settings)>) -> Vec<String> {
    let Some((streams, settings)) = view else {
      *self = Self::default();
      return Vec::new();
    };
    let mut lines = Vec::new();
    // `streams` is sorted, so the list compares and prints the same while nothing changed.
    let paired: Vec<(u64, bool, &'static str)> =
      streams.iter().map(|s| (s.owner, s.is_game, if s.is_game { "" } else { view::detector(s) })).collect();
    if self.paired.as_ref() != Some(&paired) {
      lines.push(start_line(streams, settings));
      self.written.retain(|owner, _| paired.iter().any(|p| p.0 == *owner));
      self.paired = Some(paired);
      self.settings = Some(settings_of(settings));
      self.baseline = true;
    }
    let since = *self.since.get_or_insert(now);
    for s in streams.iter().filter(|s| s.fresh) {
      if s.is_game {
        self.games.entry(s.owner).or_default().add(s);
      } else {
        self.chat.add(s, settings);
      }
    }
    let window = now.saturating_duration_since(since);
    if window < SUMMARY_EVERY {
      return lines;
    }
    if self.chat.samples > 0 || !self.games.is_empty() {
      lines.extend(self.summary(window, streams, settings));
    }
    *self = Self {
      paired: self.paired.take(),
      settings: self.settings,
      since: Some(now),
      written: std::mem::take(&mut self.written),
      ..Self::default()
    };
    lines
  }

  fn summary(&mut self, window: Duration, streams: &[StreamView], settings: &Settings) -> Vec<String> {
    let secs = format!("{:.0} s", window.as_secs_f32());
    let chat = if streams.iter().any(|s| !s.is_game) { self.chat.text() } else { "no chat stream paired".to_string() };
    let now = settings_of(settings);
    let changed = if self.settings == Some(now) { String::new() } else { format!(", {}", settings_text(now)) };
    // Speech, or chat sound the detector did not take as speech: both are worth a line.
    let talked = self.chat.speech_samples > 0 || self.chat.sound_samples > 0;
    let mut lines = Vec::new();
    if self.games.is_empty() {
      if talked || self.baseline {
        let game = if streams.iter().any(|s| s.is_game) { "game stream silent" } else { "no game stream paired" };
        lines.push(format!("{secs}: {game}, {chat}{changed}"));
      }
    } else {
      for (owner, g) in &self.games {
        let level = g.level();
        let moved = self.written.get(owner).is_none_or(|w| w.moved(&level));
        if talked || g.talking > 0 || g.lowered || moved {
          self.written.insert(*owner, level);
          lines.push(format!("{secs} {}: {}{changed}", g.exe, g.text(&chat)));
        }
      }
    }
    // A settings change is told on the next line that is written.
    if !lines.is_empty() {
      self.settings = Some(now);
    }
    lines
  }
}

impl Level {
  /// Something worth a line: the level moved (raised, or started over) or was raised again.
  fn moved(&self, now: &Level) -> bool {
    let apart = |a: i32, b: i32| if a == dbg::NA || b == dbg::NA { a != b } else { a.abs_diff(b) >= LEVEL_MOVE_X10 };
    apart(self.level, now.level) || self.raises != now.raises
  }
}

impl Stat {
  fn add(&mut self, v: i32) {
    if v == dbg::NA {
      return;
    }
    (self.min, self.max) = if self.n == 0 { (v, v) } else { (self.min.min(v), self.max.max(v)) };
    self.sum += i64::from(v);
    self.n += 1;
  }

  /// "min/avg/max dB", "-" without samples.
  fn text(&self) -> String {
    if self.n == 0 {
      return "-".to_string();
    }
    let avg = (self.sum as f64 / f64::from(self.n)) as f32 / 10.0;
    format!("{:.1}/{avg:.1}/{:.1} dB", self.min as f32 / 10.0, self.max as f32 / 10.0)
  }
}

impl ChatStats {
  fn add(&mut self, s: &StreamView, settings: &Settings) {
    let band = view::at(&s.dbg, BAND_DB);
    self.band.add(band);
    self.samples += 1;
    self.sound_samples += u32::from(band != dbg::NA && band > settings.talk_threshold_db_x10);
    self.detector |= view::detector(s) == "yes";
    let prob = view::at(&s.dbg, VAD_PROB);
    if prob >= 0 {
      self.prob_max = Some(self.prob_max.map_or(prob, |m| m.max(prob)));
    }
    if prob >= settings.speech_prob_permille {
      self.speech_samples += 1;
      self.speech.add(band);
    }
  }

  fn text(&self) -> String {
    let speech = if self.detector {
      format!(
        "speech {} % (prob max {}) at {}",
        share(self.speech_samples, self.samples),
        // A detector without a probability was not fed: the stream APO rests it while no game plays.
        self.prob_max.map_or("-, resting: no game plays".to_string(), |p| format!("{:.2}", p as f32 / 1000.0)),
        self.speech.text()
      )
    } else {
      "no detector".to_string()
    };
    format!("chat {}, sound {} %, {speech}", self.band.text(), share(self.sound_samples, self.samples))
  }
}

impl GameStats {
  fn add(&mut self, s: &StreamView) {
    self.exe.clone_from(&s.exe);
    self.band.add(view::at(&s.dbg, BAND_DB));
    let depth = view::at(&s.dbg, DEPTH_DB);
    self.depth.add(depth);
    self.lowered |= depth != dbg::NA && depth > 0;
    self.samples += 1;
    let (talking, lowering) = (view::flag(&s.dbg, FLAG_TALKING), view::flag(&s.dbg, FLAG_LOWERING));
    // Rising edges: a spurt that began before the window is not counted again.
    self.spurts += u32::from(talking && self.prev_flags & FLAG_TALKING == 0);
    self.lowered_spurts += u32::from(lowering && self.prev_flags & FLAG_LOWERING == 0);
    self.prev_flags = if view::at(&s.dbg, FLAGS) == dbg::NA { 0 } else { view::at(&s.dbg, FLAGS) };
    self.talking += u32::from(talking);
    self.lowering += u32::from(lowering);
    if talking {
      self.need.add(view::at(&s.dbg, NEED_DB));
    }
    self.last = Some(s.dbg);
    if let Some((mix, chat_band, sounding)) = s.mix {
      self.mix_level.add(mix);
      self.chat_band.add(chat_band);
      self.chat_sounding += u32::from(sounding);
    }
  }

  fn level(&self) -> Level {
    let d = self.last.unwrap_or([dbg::NA; DUCK_DBG_SLOTS]);
    Level { level: view::at(&d, LEVEL_DB), raises: view::at(&d, LEVEL_RAISES) }
  }

  fn text(&self, chat: &str) -> String {
    let d = self.last.unwrap_or([dbg::NA; DUCK_DBG_SLOTS]);
    let raises = view::at(&d, LEVEL_RAISES);
    let line = format!(
      "game {} (now {}), {chat}, spurts {} ({} lowered), talking {} %, lowering {} %, need {}, depth {}, \
       lowering level {} (raised {})",
      self.band.text(),
      one(view::at(&d, GAME_DB)),
      self.spurts,
      self.lowered_spurts,
      share(self.talking, self.samples),
      share(self.lowering, self.samples),
      self.need.text(),
      self.depth.text(),
      one(view::at(&d, LEVEL_DB)),
      if raises == dbg::NA { "-".to_string() } else { raises.to_string() },
    );
    if self.mix_level.n == 0 {
      return line;
    }
    format!(
      "{line}, mix level {}, chat band {}, chat sounding {} %",
      self.mix_level.text(),
      self.chat_band.text(),
      share(self.chat_sounding, self.mix_level.n)
    )
  }
}

fn start_line(streams: &[StreamView], settings: &Settings) -> String {
  let streams: Vec<String> = streams
    .iter()
    .map(|s| {
      let format = format!("{} {} Hz {} ch", s.exe, s.sample_rate, s.channels);
      if s.is_game { format!("game {format}") } else { format!("chat {format}, detector {}", view::detector(s)) }
    })
    .collect();
  let streams = if streams.is_empty() { "no chat or game stream paired".to_string() } else { streams.join("; ") };
  let master = if settings.master_on { "on" } else { "off" };
  format!(
    "start: {}, master switch {master}, tuning {}; {streams}",
    settings_text(settings_of(settings)),
    settings.tuning
  )
}

fn settings_of(s: &Settings) -> (u32, u32, u32) {
  (s.strength_permille, s.gains_permille.0, s.gains_permille.1)
}

fn settings_text((strength, game, chat): (u32, u32, u32)) -> String {
  format!("strength {} %, gains game {} % chat {} %", strength / 10, game / 10, chat / 10)
}

fn share(part: u32, of: u32) -> u64 {
  if of == 0 { 0 } else { u64::from(part) * 100 / u64::from(of) }
}

/// One dB x 10 value as "-22.0 dB", "-" for `dbg::NA`.
fn one(x10: i32) -> String {
  if x10 == dbg::NA { "-".to_string() } else { format!("{:.1} dB", x10 as f32 / 10.0) }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::win_audio::ducking::view::tests::stream;

  /// A chat stream (detector on) and a game stream with `flags` and lowering level `level`; lowered
  /// only while talking.
  fn streams(flags: i32, vad_prob: i32, level: i32) -> Vec<StreamView> {
    let mut chat = stream(1, false);
    // Under the talk threshold (-50 dB): only the detector decides speech.
    (chat.dbg[BAND_DB], chat.dbg[FLAGS], chat.dbg[VAD_PROB]) = (-600, dbg::FLAG_VAD, vad_prob);
    let mut game = stream(2, true);
    let depth = if flags != 0 { 61 } else { 0 };
    (game.dbg[BAND_DB], game.dbg[FLAGS], game.dbg[GAME_DB]) = (-250, flags, -220);
    (game.dbg[NEED_DB], game.dbg[DEPTH_DB]) = (90, depth);
    (game.dbg[LEVEL_DB], game.dbg[LEVEL_RAISES]) = (level, 1);
    vec![chat, game]
  }

  fn settings() -> Settings {
    Settings {
      strength_permille: 500,
      gains_permille: (1000, 800),
      master_on: true,
      tuning: "defaults".into(),
      speech_prob_permille: 500,
      talk_threshold_db_x10: -500,
    }
  }

  #[test]
  fn stat_skips_na_and_an_empty_one_prints_a_dash() {
    let mut s = Stat::default();
    assert_eq!(s.text(), "-");
    for v in [-400, dbg::NA, -200, -300] {
      s.add(v);
    }
    assert_eq!(s.text(), "-40.0/-30.0/-20.0 dB");
  }

  #[test]
  fn ten_second_line_counts_rising_edges_and_nothing_is_logged_while_off() {
    let (talk, lower) = (FLAG_TALKING, FLAG_TALKING | FLAG_LOWERING);
    let mut log = DebugLog::default();
    let t0 = Instant::now();
    let s = settings();
    // Talking, lowered, quiet, talking again (not lowered): 2 spurts, 1 lowered.
    let mut lines = Vec::new();
    for (i, f) in [0, talk, lower, lower, 0, talk, 0].iter().enumerate() {
      let v = streams(*f, if *f != 0 { 900 } else { 100 }, 120);
      lines.extend(log.tick(t0 + Duration::from_millis(100 * i as u64), Some((v.as_slice(), &s))));
    }
    assert_eq!(lines.len(), 1, "only the start line before 10 s: {lines:?}");
    assert_eq!(
      lines[0],
      "start: strength 50 %, gains game 100 % chat 80 %, master switch on, tuning defaults; chat Discord.exe 48000 Hz \
       2 ch, detector yes; game game.exe 48000 Hz 2 ch"
    );

    let v = streams(0, 100, 120);
    let lines = log.tick(t0 + SUMMARY_EVERY, Some((v.as_slice(), &s)));
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert!(line.starts_with("10 s game.exe: game -25.0/-25.0/-25.0 dB (now -22.0 dB)"), "{line}");
    assert!(line.contains("sound 0 %, speech 50 % (prob max 0.90) at -60.0/-60.0/-60.0 dB"), "{line}");
    assert!(line.contains("spurts 2 (1 lowered), talking 50 %, lowering 25 %"), "{line}");
    assert!(line.ends_with("lowering level 12.0 dB (raised 1)"), "{line}");

    // Off: nothing, and the next tick on starts over with a start line.
    assert!(log.tick(t0 + SUMMARY_EVERY * 3, None).is_empty());
    let again = log.tick(t0 + SUMMARY_EVERY * 4, Some((v.as_slice(), &s)));
    assert!(again.len() == 1 && again[0].starts_with("start:"), "{again:?}");
  }

  #[test]
  fn a_window_is_written_only_when_something_happens() {
    let s = settings();
    let mut log = DebugLog::default();
    let t0 = Instant::now();
    let at_window = |n: u32| t0 + SUMMARY_EVERY * n;
    let mut tick =
      |n: u32, flags: i32, level: i32| log.tick(at_window(n), Some((streams(flags, 100, level).as_slice(), &s))).len();
    assert_eq!(tick(0, 0, 120), 1, "the start line");
    assert_eq!(tick(1, 0, 120), 1, "the first window is the baseline");
    assert_eq!(tick(2, 0, 120), 0, "game only, nothing moved: dropped");
    assert_eq!(tick(3, 0, 125), 0, "under 1 dB is not news");
    assert_eq!(tick(4, 0, 0), 1, "the level started over");
    assert_eq!(tick(5, 0, 0), 0);
    assert_eq!(tick(6, FLAG_TALKING, 0), 1, "someone talked");
    assert_eq!(tick(7, 0, 0), 0);
    let mut sound = streams(0, 100, 0);
    sound[0].dbg[BAND_DB] = -300;
    assert_eq!(log.tick(at_window(8), Some((sound.as_slice(), &s))).len(), 1, "chat sound without speech");
    assert_eq!(log.tick(at_window(9), Some((streams(0, 100, 0).as_slice(), &s))).len(), 0, "silent chat");
    let mut raised = streams(0, 100, 0);
    raised[1].dbg[LEVEL_RAISES] = 2;
    assert_eq!(log.tick(at_window(10), Some((raised.as_slice(), &s))).len(), 1, "raised again");
  }

  #[test]
  fn garbage_from_the_other_process_does_not_overflow() {
    let wild = Level { level: i32::MAX, raises: i32::MIN + 1 };
    let other = Level { level: i32::MIN + 1, raises: i32::MAX };
    assert!(other.moved(&wild) && wild.moved(&other));
    assert!(!wild.moved(&wild));
  }

  #[test]
  fn the_start_line_waits_for_the_detector_and_ignores_order() {
    let s = settings();
    let mut log = DebugLog::default();
    let t0 = Instant::now();
    let mut v = streams(0, 100, 120);
    v[0].dbg[FLAGS] = dbg::NA;
    let first = log.tick(t0, Some((v.as_slice(), &s)));
    assert!(first[0].contains("detector ?"), "{first:?}");
    v[0].dbg[FLAGS] = dbg::FLAG_VAD;
    let known = log.tick(t0 + Duration::from_millis(100), Some((v.as_slice(), &s)));
    assert!(known.len() == 1 && known[0].contains("detector yes"), "{known:?}");
    assert!(log.tick(t0 + Duration::from_millis(200), Some((v.as_slice(), &s))).is_empty(), "unchanged: no start line");
  }
}
