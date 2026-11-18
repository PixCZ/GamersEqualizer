//! The per-minute info line: for every minute in which someone talked, the talk spurts, how many
//! lowered the game and how far, or when none did, the numbers that say why; for a minute with chat
//! sound the detector never took as speech, that. A silent chat writes nothing. Fed every 100 ms by
//! `Ducking::tick`.

use gamersequalizer_common::ducking::dbg::{self, BAND_DB, FLAG_TALKING, NEED_DB, VAD_PROB};
use std::time::{Duration, Instant};

use super::view::{self, Settings, StreamView};

const PERIOD: Duration = Duration::from_secs(60);

/// What one tick contributes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
  /// A playing game stream hears someone talk.
  pub talking: bool,
  /// How far the game is lowered now (permille).
  pub reduction: u32,
  /// A game stream plays; the most lowered one's speech-band mean square and learned levels.
  pub has_game: bool,
  pub game_speech_ms: f32,
  pub learned: (i32, i32),
  /// Highest need over the talking game streams (dB x 10, `dbg::NA` = none).
  pub need: i32,
  /// Chat streams sampled, of them taken as speech, and their highest detector probability
  /// (x 1000; None without a detector).
  pub chat_samples: u32,
  pub chat_speech: u32,
  pub prob: Option<i32>,
  /// Chat streams sampled whose band level is above the talk threshold (speech or not).
  pub chat_sound: u32,
  /// The most lowered game is mix mode's (`MixRun::view`), and the chat made sound (its level held).
  pub mix: bool,
  pub chat_sounding: bool,
}

#[derive(Debug, Default)]
pub struct MinuteSummary {
  started: Option<Instant>,
  last_tick: Option<Instant>,
  /// A spurt: someone talks or the game is still lowered after it.
  spurts: u32,
  lowered_spurts: u32,
  in_spurt: bool,
  /// Highest reduction of the current spurt (permille, 0 = not lowered).
  spurt_peak: u32,
  /// Lowest and highest peak of the lowered spurts (permille).
  peaks: Option<(u32, u32)>,
  lowered_time: Duration,
  /// Game speech-band level while lowered: sum and count of the per-tick dB.
  game_db_sum: f64,
  game_db_n: u32,
  learned: (i32, i32),
  need_max: Option<i32>,
  chat_samples: u32,
  chat_speech: u32,
  chat_sound: u32,
  prob_max: Option<i32>,
  /// Mix mode lowered the game this minute; its ticks and those with the chat sounding.
  mix: bool,
  mix_ticks: u32,
  sounding_ticks: u32,
}

impl Sample {
  pub fn read(streams: &[StreamView], settings: &Settings) -> Self {
    let games = || streams.iter().filter(|s| s.is_game && s.fresh);
    let talking = |s: &&StreamView| view::flag(&s.dbg, FLAG_TALKING);
    let main = view::main_game(streams);
    let chats: Vec<&StreamView> = streams.iter().filter(|s| !s.is_game && s.fresh).collect();
    let probs = chats.iter().map(|s| view::at(&s.dbg, VAD_PROB)).filter(|p| *p >= 0);
    Self {
      talking: games().any(|s| talking(&s)),
      reduction: main.map_or(0, |s| s.duck_permille),
      has_game: main.is_some(),
      game_speech_ms: main.map_or(0.0, |s| s.speech_ms),
      learned: main.map_or((0, 0), |s| s.learned),
      mix: main.is_some_and(|s| s.mix.is_some()),
      chat_sounding: main.and_then(|s| s.mix).is_some_and(|(_, _, sounding)| sounding),
      need: games()
        .filter(talking)
        .map(|s| view::at(&s.dbg, NEED_DB))
        .filter(|n| *n != dbg::NA)
        .max()
        .unwrap_or(dbg::NA),
      chat_samples: chats.len() as u32,
      chat_speech: chats.iter().filter(|s| view::at(&s.dbg, VAD_PROB) >= settings.speech_prob_permille).count() as u32,
      prob: probs.max(),
      chat_sound: chats
        .iter()
        .map(|s| view::at(&s.dbg, BAND_DB))
        .filter(|b| *b != dbg::NA && *b > settings.talk_threshold_db_x10)
        .count() as u32,
    }
  }
}

impl MinuteSummary {
  /// One tick while the feature is on; the line to log when a minute with talk ended.
  pub fn tick(&mut self, now: Instant, streams: &[StreamView], settings: &Settings) -> Option<String> {
    self.observe(now, &Sample::read(streams, settings), settings.gains_permille.1)
  }

  fn observe(&mut self, now: Instant, s: &Sample, chat_gain_permille: u32) -> Option<String> {
    let started = *self.started.get_or_insert(now);
    let since_last = self.last_tick.map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
    self.last_tick = Some(now);
    let active = s.talking || s.reduction > 0;
    if active && !self.in_spurt {
      (self.in_spurt, self.spurt_peak, self.spurts) = (true, 0, self.spurts + 1);
    } else if !active && self.in_spurt {
      self.in_spurt = false;
      self.end_spurt();
    }
    if s.reduction > 0 {
      self.spurt_peak = self.spurt_peak.max(s.reduction);
      // Shortcut: the whole interval up to a lowered tick counts, +-one tick (100 ms) per spurt.
      self.lowered_time += since_last;
      if s.game_speech_ms > 0.0 {
        self.game_db_sum += 10.0 * f64::from(s.game_speech_ms).log10();
        self.game_db_n += 1;
      }
    }
    if s.has_game {
      self.learned = s.learned;
    }
    self.mix |= s.mix && s.reduction > 0;
    if s.mix {
      self.mix_ticks += 1;
      self.sounding_ticks += u32::from(s.chat_sounding);
    }
    if s.talking && s.need != dbg::NA {
      self.need_max = Some(self.need_max.map_or(s.need, |m| m.max(s.need)));
    }
    self.chat_samples += s.chat_samples;
    self.chat_speech += s.chat_speech;
    self.chat_sound += s.chat_sound;
    if let Some(p) = s.prob {
      self.prob_max = Some(self.prob_max.map_or(p, |m| m.max(p)));
    }
    // A spurt still going on keeps the minute open, so each one is counted once, with its full
    // depth. Shortcut: a spurt that never ends (constant talk over constant fire) delays the line.
    if self.in_spurt || now.saturating_duration_since(started) < PERIOD {
      return None;
    }
    let line = (self.spurts > 0 || self.chat_speech > 0 || self.chat_sound > 0)
      .then(|| self.line(chat_gain_permille, now.saturating_duration_since(started)));
    *self = Self { started: Some(now), last_tick: Some(now), ..Self::default() };
    line
  }

  fn end_spurt(&mut self) {
    let p = self.spurt_peak;
    if p > 0 {
      self.lowered_spurts += 1;
      self.peaks = Some(self.peaks.map_or((p, p), |(lo, hi)| (lo.min(p), hi.max(p))));
    }
  }

  /// `span` is what the line covers: longer than `PERIOD` when a spurt held the minute open.
  fn line(&self, chat_gain_permille: u32, span: Duration) -> String {
    let last = if span < PERIOD * 3 / 2 { "minute".to_string() } else { format!("{} s", span.as_secs()) };
    let prob = self.prob_max.map_or("no voice detector".to_string(), |p| format!("prob max {:.2}", p as f32 / 1000.0));
    if self.spurts == 0 && self.chat_speech == 0 {
      let sound = share(self.chat_sound, self.chat_samples);
      return format!("last {last}: chat sound without speech {sound} % of samples ({prob}), nothing lowered");
    }
    let mix = if self.mix { " (mix)" } else { "" };
    let mut parts = vec![format!("last {last}{mix}: {} spurts", self.spurts)];
    if self.lowered_spurts > 0 {
      let (lo, hi) = self.peaks.unwrap_or_default();
      let game = if self.game_db_n == 0 {
        "-".to_string()
      } else {
        format!("{:.1} dB", self.game_db_sum / f64::from(self.game_db_n))
      };
      parts.push(format!(
        "{} lowered for {:.0} s, depth {:.1}-{:.1} dB, game {game} while lowered",
        self.lowered_spurts,
        self.lowered_time.as_secs_f32(),
        depth_db(lo),
        depth_db(hi)
      ));
    } else {
      parts.push("none lowered".to_string());
      parts.push(format!("chat speech {} % of samples ({prob})", share(self.chat_speech, self.chat_samples)));
      // Negative: the voice was already above the game.
      parts.push(self.need_max.map_or("need max -".to_string(), |n| format!("need max {:+.1} dB", n as f32 / 10.0)));
      if chat_gain_permille == 0 {
        parts.push("chat gain 0 %".to_string());
      }
    }
    if self.mix {
      // Those ticks did not update the game's level: it stands still while the chat sounds.
      parts.push(format!("chat sounding {} %", share(self.sounding_ticks, self.mix_ticks)));
    }
    parts.push(super::levels_text(self.learned));
    parts.join(", ")
  }
}

/// A reduction as dB of attenuation: 750 permille (to a quarter of the amplitude) = 12.0 dB.
fn depth_db(permille: u32) -> f32 {
  -20.0 * (1.0 - permille.min(999) as f32 / 1000.0).log10()
}

fn share(part: u32, of: u32) -> u64 {
  if of == 0 { 0 } else { u64::from(part) * 100 / u64::from(of) }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample(talking: bool, reduction: u32) -> Sample {
    Sample {
      talking,
      reduction,
      has_game: true,
      game_speech_ms: 0.01,
      learned: (-205, -330),
      need: if talking { -30 } else { dbg::NA },
      chat_samples: 1,
      chat_speech: u32::from(talking),
      prob: Some(if talking { 900 } else { 100 }),
      chat_sound: 0,
      mix: false,
      chat_sounding: false,
    }
  }

  /// One 100 ms tick per entry of (talking, reduction) from `t`; the lines and the time after.
  fn run(summary: &mut MinuteSummary, t: Instant, ticks: &[(bool, u32)]) -> (Vec<String>, Instant) {
    let mut lines = Vec::new();
    let mut now = t;
    for (talking, reduction) in ticks {
      lines.extend(summary.observe(now, &sample(*talking, *reduction), 800));
      now += Duration::from_millis(100);
    }
    (lines, now)
  }

  #[test]
  fn spurts_are_counted_once_and_idle_minutes_stay_silent() {
    let mut s = MinuteSummary::default();
    // Two lowered spurts (peaks 500 and 750, the second outlasting the talk), then quiet.
    let mut minute = vec![(false, 0), (true, 300), (true, 500), (false, 400), (false, 0), (true, 750), (false, 750)];
    minute.resize(600, (false, 0));
    let (lines, t) = run(&mut s, Instant::now(), &minute);
    assert!(lines.is_empty(), "no line before the minute is over");
    // A third spurt starts at the boundary: the minute closes once it ends, counting it once.
    let (lines, t) = run(&mut s, t, &[(true, 600), (false, 0)]);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert!(line.starts_with("last minute: 3 spurts, 3 lowered for"), "{line}");
    assert!(line.contains("depth 6.0-12.0 dB, game -20.0 dB while lowered"), "{line}");
    assert!(line.ends_with("loud level -20.5 dB, quiet talker -33.0 dB"), "{line}");
    // A minute without talk: nothing.
    let (lines, _) = run(&mut s, t, &[(false, 0); 700]);
    assert!(lines.is_empty(), "{lines:?}");
  }

  #[test]
  fn a_minute_with_talk_but_nothing_lowered_says_why() {
    let mut s = MinuteSummary::default();
    let mut minute = vec![(true, 0), (true, 0), (false, 0), (false, 0)];
    minute.resize(601, (false, 0));
    let (lines, _) = run(&mut s, Instant::now(), &minute);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert!(
      line.starts_with("last minute: 1 spurts, none lowered, chat speech 0 % of samples (prob max 0.90)"),
      "{line}"
    );
    assert!(line.contains("need max -3.0 dB"), "{line}");
  }

  #[test]
  fn chat_sound_the_detector_never_took_as_speech_is_told_and_silence_is_not() {
    let t0 = Instant::now();
    let sound = Sample { talking: false, chat_sound: 1, prob: Some(120), ..sample(false, 0) };
    let mut s = MinuteSummary::default();
    let mut lines = Vec::new();
    for i in 0..=600u32 {
      let tick = if i % 2 == 0 { sound } else { sample(false, 0) };
      lines.extend(s.observe(t0 + Duration::from_millis(100 * u64::from(i)), &tick, 800));
    }
    assert_eq!(lines, ["last minute: chat sound without speech 50 % of samples (prob max 0.12), nothing lowered"]);
    // A silent chat: nothing.
    let (lines, _) = run(&mut s, t0 + Duration::from_secs(61), &[(false, 0); 700]);
    assert!(lines.is_empty(), "{lines:?}");
  }
}
