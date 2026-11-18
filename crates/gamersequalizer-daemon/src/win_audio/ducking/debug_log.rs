//! The debug lines (`[Ducking]` at debug level, like `[Chain]`): a start line with the settings
//! and the paired streams, then per 10 s window one line per game stream with what the ducker saw
//! and decided (`StreamSlot::duck_dbg`, written by the APO every block). One real call must leave
//! everything tuning needs in the log. Fed every 100 ms by `Ducking::tick`, it samples every tenth
//! 10 ms block: min/max are of those samples, not of every block.
//!
//! Written only when something happened (disk wear: no heartbeat). A window's line is kept when
//! someone talked (game stream talking or chat speech), the chat carried sound the detector did not
//! take as speech, the game was lowered, the loud level rose or the quiet talker or the lock moved
//! by 1 dB or more since the last line written for that
//! stream, its settled / stored state changed, or it is the stream's first window after its start
//! line (one baseline per session). The slow fall of the loud level does not count. Otherwise the
//! window is dropped: an idle evening writes nothing.

use gamersequalizer_common::ducking::dbg::{
  self, BAND_DB, DEPTH_DB, FLAG_LATCHED, FLAG_LOUD_SEEDED, FLAG_LOUD_SETTLED, FLAG_QUIET_SEEDED, FLAG_QUIET_SETTLED,
  FLAG_TALKING, FLAGS, GAME_DB, LOCKED_DIFF_DB, LOUD_EVIDENCE_S, NEED_DB, QUIET_EVIDENCE_S, TARGET_DEPTH_DB, VAD_PROB,
};
use gamersequalizer_common::streams::DUCK_DBG_SLOTS;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::view::{self, Settings, StreamView};

const SUMMARY_EVERY: Duration = Duration::from_secs(10);
/// A learned value moving this much (dB x 10) since the last written line keeps a window.
const LEARNED_MOVE_X10: u32 = 10;

#[derive(Debug, Default)]
pub struct DebugLog {
  /// (owner, is game, detector) of the paired streams at the last start line; None = not started.
  paired: Option<Vec<(u64, bool, &'static str)>>,
  /// (strength, game gain, chat gain) last written.
  settings: Option<(u32, u32, u32)>,
  since: Option<Instant>,
  chat: ChatStats,
  games: BTreeMap<u64, GameStats>,
  /// Per game stream: its learned values as of the last line written for it. A stream missing
  /// here gets its baseline line.
  written: BTreeMap<u64, Learned>,
  /// A start line was written and no window closed since: the chat-only line is a baseline too.
  baseline: bool,
}

/// What a game stream has learned, compared between written lines.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Learned {
  loud: i32,
  quiet: i32,
  lock: i32,
  /// The settled / stored bits of `FLAGS`.
  states: i32,
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
  latched: u32,
  spurts: u32,
  latches: u32,
  /// Some sample had a depth above 0.
  lowered: bool,
  prev_flags: i32,
  /// The last sample's values and learned levels.
  last: Option<([i32; DUCK_DBG_SLOTS], (i32, i32))>,
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
        let learned = g.learned();
        let moved = self.written.get(owner).is_none_or(|w| w.moved(&learned));
        if talked || g.talking > 0 || g.lowered || moved {
          self.written.insert(*owner, learned);
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

impl Learned {
  /// Something worth a line: the loud level rose (its slow fall is not news), the quiet talker or
  /// the lock moved, or a level settled or came from the store. The lock is the other process's raw
  /// value, so nothing here can overflow.
  fn moved(&self, now: &Learned) -> bool {
    let rose = |a: i32, b: i32| if a == 0 || b == 0 { a != b } else { b > a && a.abs_diff(b) >= LEARNED_MOVE_X10 };
    let apart = |a: i32, b: i32| if a == dbg::NA || b == dbg::NA { a != b } else { a.abs_diff(b) >= LEARNED_MOVE_X10 };
    rose(self.loud, now.loud) || apart(self.quiet, now.quiet) || apart(self.lock, now.lock) || self.states != now.states
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
        self.prob_max.map_or("-".to_string(), |p| format!("{:.2}", p as f32 / 1000.0)),
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
    let (talking, latched) = (view::flag(&s.dbg, FLAG_TALKING), view::flag(&s.dbg, FLAG_LATCHED));
    // Rising edges: a spurt or latch that began before the window is not counted again.
    self.spurts += u32::from(talking && self.prev_flags & FLAG_TALKING == 0);
    self.latches += u32::from(latched && self.prev_flags & FLAG_LATCHED == 0);
    self.prev_flags = if view::at(&s.dbg, FLAGS) == dbg::NA { 0 } else { view::at(&s.dbg, FLAGS) };
    self.talking += u32::from(talking);
    self.latched += u32::from(latched);
    if talking {
      self.need.add(view::at(&s.dbg, NEED_DB));
    }
    self.last = Some((s.dbg, s.learned));
    if let Some((mix, chat_band, sounding)) = s.mix {
      self.mix_level.add(mix);
      self.chat_band.add(chat_band);
      self.chat_sounding += u32::from(sounding);
    }
  }

  fn learned(&self) -> Learned {
    let (d, (loud, quiet)) = self.last.unwrap_or(([dbg::NA; DUCK_DBG_SLOTS], (0, 0)));
    let mask = FLAG_LOUD_SETTLED | FLAG_LOUD_SEEDED | FLAG_QUIET_SETTLED | FLAG_QUIET_SEEDED;
    let flags = view::at(&d, FLAGS);
    let states = if flags == dbg::NA { 0 } else { flags & mask };
    Learned { loud, quiet, lock: view::at(&d, LOCKED_DIFF_DB), states }
  }

  fn text(&self, chat: &str) -> String {
    let (d, (loud, quiet)) = self.last.unwrap_or(([dbg::NA; DUCK_DBG_SLOTS], (0, 0)));
    let line = format!(
      "game {} (now {}), {chat}, spurts {} ({} latched), talking {} %, latched {} %, need {}, depth {} \
       (target {}), loud level {}, quiet talker {}, lock {}",
      self.band.text(),
      one(view::at(&d, GAME_DB)),
      self.spurts,
      self.latches,
      share(self.talking, self.samples),
      share(self.latched, self.samples),
      self.need.text(),
      self.depth.text(),
      one(view::at(&d, TARGET_DEPTH_DB)),
      learned_text(
        loud,
        view::at(&d, LOUD_EVIDENCE_S),
        view::flag(&d, FLAG_LOUD_SETTLED),
        view::flag(&d, FLAG_LOUD_SEEDED)
      ),
      learned_text(
        quiet,
        view::at(&d, QUIET_EVIDENCE_S),
        view::flag(&d, FLAG_QUIET_SETTLED),
        view::flag(&d, FLAG_QUIET_SEEDED)
      ),
      one(view::at(&d, LOCKED_DIFF_DB)),
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
      let what = if s.is_game {
        format!("stored {}", if s.saved == (0, 0) { "none".to_string() } else { super::levels_text(s.saved) })
      } else {
        format!("detector {}", view::detector(s))
      };
      let kind = if s.is_game { "game" } else { "chat" };
      format!("{kind} {} {} Hz {} ch, {what}", s.exe, s.sample_rate, s.channels)
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

/// "-20.5 dB (45 s, settled, stored)"
fn learned_text(x10: i32, evidence_s: i32, settled: bool, stored: bool) -> String {
  let evidence = if evidence_s == dbg::NA { "-".to_string() } else { evidence_s.to_string() };
  let mut notes = vec![format!("{evidence} s")];
  if settled {
    notes.push("settled".to_string());
  }
  if stored {
    notes.push("stored".to_string());
  }
  format!("{} ({})", super::level_text(x10), notes.join(", "))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::win_audio::ducking::view::tests::stream;

  /// A chat stream (detector on) and a game stream with `flags`; lowered only while talking.
  fn streams(flags: i32, vad_prob: i32, loud: i32) -> Vec<StreamView> {
    let mut chat = stream(1, false);
    // Under the talk threshold (-50 dB): only the detector decides speech.
    (chat.dbg[BAND_DB], chat.dbg[FLAGS], chat.dbg[VAD_PROB]) = (-600, dbg::FLAG_VAD, vad_prob);
    let mut game = stream(2, true);
    let depth = if flags != 0 { 61 } else { 0 };
    (game.dbg[BAND_DB], game.dbg[FLAGS], game.dbg[GAME_DB]) = (-250, flags, -220);
    (game.dbg[NEED_DB], game.dbg[DEPTH_DB], game.dbg[TARGET_DEPTH_DB]) = (90, depth, 120);
    game.learned = (loud, -330);
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
    let (talk, latch) = (FLAG_TALKING, FLAG_TALKING | FLAG_LATCHED);
    let mut log = DebugLog::default();
    let t0 = Instant::now();
    let s = settings();
    // Talking, latched, quiet, talking again (not latched): 2 spurts, 1 latch.
    let mut lines = Vec::new();
    for (i, f) in [0, talk, latch, latch, 0, talk, 0].iter().enumerate() {
      let v = streams(*f, if *f != 0 { 900 } else { 100 }, -205);
      lines.extend(log.tick(t0 + Duration::from_millis(100 * i as u64), Some((v.as_slice(), &s))));
    }
    assert_eq!(lines.len(), 1, "only the start line before 10 s: {lines:?}");
    assert_eq!(
      lines[0],
      "start: strength 50 %, gains game 100 % chat 80 %, master switch on, tuning defaults; chat Discord.exe 48000 Hz \
       2 ch, detector yes; game game.exe 48000 Hz 2 ch, stored none"
    );

    let v = streams(0, 100, -205);
    let lines = log.tick(t0 + SUMMARY_EVERY, Some((v.as_slice(), &s)));
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert!(line.starts_with("10 s game.exe: game -25.0/-25.0/-25.0 dB (now -22.0 dB)"), "{line}");
    assert!(line.contains("sound 0 %, speech 50 % (prob max 0.90) at -60.0/-60.0/-60.0 dB"), "{line}");
    assert!(line.contains("spurts 2 (1 latched), talking 50 %, latched 25 %"), "{line}");
    assert!(
      line.contains("(target 12.0 dB), loud level -20.5 dB (- s), quiet talker -33.0 dB (- s), lock -"),
      "{line}"
    );

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
      |n: u32, flags: i32, loud: i32| log.tick(at_window(n), Some((streams(flags, 100, loud).as_slice(), &s))).len();
    assert_eq!(tick(0, 0, -205), 1, "the start line");
    assert_eq!(tick(1, 0, -205), 1, "the first window is the baseline");
    assert_eq!(tick(2, 0, -205), 0, "game only, nothing learned: dropped");
    assert_eq!(tick(3, 0, -215), 0, "the loud level's slow fall is not news");
    assert_eq!(tick(4, 0, -195), 1, "the loud level rose 1 dB");
    assert_eq!(tick(5, 0, -195), 0);
    assert_eq!(tick(6, FLAG_TALKING, -195), 1, "someone talked");
    assert_eq!(tick(7, 0, -195), 0);
    let mut sound = streams(0, 100, -195);
    sound[0].dbg[BAND_DB] = -300;
    assert_eq!(log.tick(at_window(8), Some((sound.as_slice(), &s))).len(), 1, "chat sound without speech");
    assert_eq!(log.tick(at_window(9), Some((streams(0, 100, -195).as_slice(), &s))).len(), 0, "silent chat");
  }

  #[test]
  fn garbage_from_the_other_process_does_not_overflow() {
    let wild = Learned { loud: i32::MAX, quiet: i32::MIN + 1, lock: i32::MAX, states: 0 };
    let other = Learned { loud: i32::MIN + 1, quiet: i32::MAX, lock: i32::MIN + 1, states: 0 };
    assert!(other.moved(&wild) && wild.moved(&other));
    assert!(!wild.moved(&wild));
  }

  #[test]
  fn the_start_line_waits_for_the_detector_and_ignores_order() {
    let s = settings();
    let mut log = DebugLog::default();
    let t0 = Instant::now();
    let mut v = streams(0, 100, -205);
    v[0].dbg[FLAGS] = dbg::NA;
    let first = log.tick(t0, Some((v.as_slice(), &s)));
    assert!(first[0].contains("detector ?"), "{first:?}");
    v[0].dbg[FLAGS] = dbg::FLAG_VAD;
    let known = log.tick(t0 + Duration::from_millis(100), Some((v.as_slice(), &s)));
    assert!(known.len() == 1 && known[0].contains("detector yes"), "{known:?}");
    assert!(log.tick(t0 + Duration::from_millis(200), Some((v.as_slice(), &s))).is_empty(), "unchanged: no start line");
  }
}
