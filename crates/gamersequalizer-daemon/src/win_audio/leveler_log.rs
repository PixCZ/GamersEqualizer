//! Debug log of the voice leveler: one line per change of its gain or learned level (0.1 dB
//! steps, as published), with why it moved. The DSP (APO or bridge) has no log of its own, so the
//! daemon diffs the shared telemetry and the settings that feed the leveler.
//!
//! Shortcut: the reason is inferred from what the daemon sees, not reported by the DSP. Speech the
//! leveler skipped (foreign voices, clamped outliers) is not counted, and a gain move without any
//! visible cause is reported as "no visible cause" (the ~100 ms ramp of an earlier change, or a
//! remembered level applied at stream start). Upgrade with DSP-side counters in the next shm layout.

use gamersequalizer_common::shm::ShmHeader;
use log::debug;
use std::sync::atomic::Ordering;

/// Gain caps of the leveler (`voice_leveler.rs`), dB x 10.
const MAX_BOOST_X10: i32 = 240;
const MAX_CUT_X10: i32 = -60;

/// What feeds or shows the leveler, read once per tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LevelerSnapshot {
  pub enabled: bool,
  pub bypass: bool,
  pub gain_x10: i32,
  /// 0 = nothing learned yet.
  pub level_x10: i32,
  pub speech_ms: u32,
  pub input_gain_x10: i32,
  pub target_x10: i32,
  pub reset_seq: u32,
  /// Settings before the leveler: when they change, it relearns fast (`plugin.rs` LevelerInputs).
  pub aec_enabled: bool,
  pub ai_strength_permille: u32,
  pub gate_db: i32,
  pub model_active: u32,
  pub eq_version: u32,
}

#[derive(Debug, Default)]
pub struct LevelerLog {
  /// Last tick, and the gain/level last logged.
  last: Option<LevelerSnapshot>,
  logged: Option<(i32, i32)>,
}

impl LevelerSnapshot {
  pub fn read(h: &ShmHeader) -> Self {
    let r = Ordering::Relaxed;
    Self {
      enabled: h.mic_leveler_enabled.load(r),
      bypass: h.master_bypass.load(r),
      gain_x10: h.mic_leveler_gain_db_x10.load(r),
      level_x10: h.mic_leveler_level_db_x10.load(r),
      speech_ms: h.mic_speech_ms.load(r),
      input_gain_x10: h.mic_input_gain_db_x10.load(r),
      target_x10: h.mic_leveler_target_db_x10.load(r),
      reset_seq: h.mic_leveler_reset_seq.load(r),
      aec_enabled: h.aec_enabled.load(r),
      ai_strength_permille: h.ai_voice_strength_permille.load(r),
      gate_db: h.noise_gate_threshold_db.load(r),
      model_active: h.denoise_model_active.load(r),
      eq_version: h.mic_eq_version.load(r),
    }
  }
}

impl LevelerLog {
  /// Once per volume sync (~250 ms), while the mic is processed.
  pub fn tick(&mut self, h: &ShmHeader) {
    if !log::log_enabled!(log::Level::Debug) {
      self.last = None; // debug off: start fresh when it is switched on
      return;
    }
    if let Some(line) = self.observe(LevelerSnapshot::read(h)) {
      debug!("[Leveler] {line}");
    }
  }

  /// The log line for this tick, if the gain or the level moved (or the leveler was switched).
  pub fn observe(&mut self, now: LevelerSnapshot) -> Option<String> {
    let Some(prev) = self.last.replace(now) else {
      self.logged = Some((now.gain_x10, now.level_x10));
      return Some(format!("start: {}", state(&now)));
    };
    let reasons = reasons(&prev, &now);
    let (gain0, level0) = self.logged.unwrap_or((prev.gain_x10, prev.level_x10));
    let moved = now.gain_x10 != gain0 || now.level_x10 != level0;
    if !moved && !reasons.iter().any(|r| r.always_logged) {
      return None;
    }
    self.logged = Some((now.gain_x10, now.level_x10));
    let why = if reasons.is_empty() {
      "no visible cause (ramp of an earlier change, or a remembered level)".to_string()
    } else {
      reasons.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join(", ")
    };
    let cap = match now.gain_x10 {
      g if g >= MAX_BOOST_X10 => " [at the +24 dB boost cap]",
      g if g <= MAX_CUT_X10 => " [at the -6 dB cut cap]",
      _ => "",
    };
    Some(format!(
      "gain {} -> {} dB, level {} -> {} dBFS{cap} | why: {why}",
      db(gain0),
      db(now.gain_x10),
      level(level0),
      level(now.level_x10),
    ))
  }
}

struct Reason {
  text: String,
  /// Logged even when the gain didn't move (a switch the user made).
  always_logged: bool,
}

fn reasons(prev: &LevelerSnapshot, now: &LevelerSnapshot) -> Vec<Reason> {
  let mut out = Vec::new();
  let mut add = |text: String, always_logged| out.push(Reason { text, always_logged });
  if prev.enabled != now.enabled {
    add(format!("leveler switched {}", if now.enabled { "on" } else { "off (gain 0 dB)" }), true);
  }
  if prev.bypass != now.bypass {
    add(format!("bypass {}", if now.bypass { "on" } else { "off" }), true);
  }
  if prev.reset_seq != now.reset_seq {
    add("\"Learn again\": learned level forgotten".to_string(), true);
  }
  if now.speech_ms < prev.speech_ms {
    add("new stream (an app reopened the mic): starts from the remembered level".to_string(), true);
  } else if now.speech_ms > prev.speech_ms {
    add(format!("learned from {} ms of speech", now.speech_ms - prev.speech_ms), false);
  }
  if prev.input_gain_x10 != now.input_gain_x10 {
    add(format!("Windows mic volume {} -> {} dB (compensated)", db(prev.input_gain_x10), db(now.input_gain_x10)), true);
  }
  if prev.target_x10 != now.target_x10 {
    add(format!("target {} -> {} dBFS", level(prev.target_x10), level(now.target_x10)), true);
  }
  let mut changed = Vec::new();
  if prev.aec_enabled != now.aec_enabled {
    changed.push("echo cancellation");
  }
  if prev.ai_strength_permille != now.ai_strength_permille {
    changed.push("denoiser strength");
  }
  if prev.gate_db != now.gate_db {
    changed.push("noise gate");
  }
  if prev.model_active != now.model_active {
    changed.push("denoiser model");
  }
  if prev.eq_version != now.eq_version {
    changed.push("voice EQ");
  }
  if !changed.is_empty() {
    add(format!("{} changed: learning fast again", changed.join(" + ")), true);
  }
  out
}

fn db(x10: i32) -> String {
  format!("{:+.1}", x10 as f32 / 10.0)
}

fn level(x10: i32) -> String {
  if x10 == 0 {
    "none".to_string()
  } else {
    format!("{:.1}", x10 as f32 / 10.0)
  }
}

fn state(s: &LevelerSnapshot) -> String {
  format!(
    "leveler {}, gain {} dB, level {} dBFS, target {} dBFS, Windows mic volume {} dB",
    if s.enabled { "on" } else { "off" },
    db(s.gain_x10),
    level(s.level_x10),
    level(s.target_x10),
    db(s.input_gain_x10)
  )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
  use super::*;

  fn snap() -> LevelerSnapshot {
    LevelerSnapshot {
      enabled: true,
      gain_x10: 60,
      level_x10: -300,
      speech_ms: 1000,
      target_x10: -240,
      ..Default::default()
    }
  }

  fn started() -> LevelerLog {
    let mut log = LevelerLog::default();
    assert!(log.observe(snap()).unwrap().starts_with("start:"));
    log
  }

  #[test]
  fn nothing_moved_nothing_logged() {
    let mut log = started();
    assert_eq!(log.observe(snap()), None);
  }

  #[test]
  fn speech_that_moves_the_gain_is_logged_with_its_reason() {
    let mut log = started();
    let line = log.observe(LevelerSnapshot { gain_x10: 64, level_x10: -304, speech_ms: 1250, ..snap() }).unwrap();
    assert!(line.contains("gain +6.0 -> +6.4 dB"), "{line}");
    assert!(line.contains("level -30.0 -> -30.4 dBFS"), "{line}");
    assert!(line.contains("learned from 250 ms of speech"), "{line}");
  }

  #[test]
  fn speech_without_a_move_is_not_logged() {
    let mut log = started();
    assert_eq!(log.observe(LevelerSnapshot { speech_ms: 1250, ..snap() }), None);
  }

  #[test]
  fn a_ramp_after_a_change_compares_with_the_last_logged_value() {
    let mut log = started();
    log.observe(LevelerSnapshot { input_gain_x10: -20, gain_x10: 70, ..snap() }).unwrap();
    // Next tick the gain still ramps, no visible cause.
    let line = log.observe(LevelerSnapshot { input_gain_x10: -20, gain_x10: 80, ..snap() }).unwrap();
    assert!(line.contains("gain +7.0 -> +8.0 dB"), "{line}");
    assert!(line.contains("no visible cause"), "{line}");
  }

  #[test]
  fn switches_are_logged_even_without_a_move() {
    let mut log = started();
    let line = log.observe(LevelerSnapshot { gate_db: -50, eq_version: 3, ..snap() }).unwrap();
    assert!(line.contains("noise gate + voice EQ changed"), "{line}");
    let line = log.observe(LevelerSnapshot { gate_db: -50, eq_version: 3, reset_seq: 1, ..snap() }).unwrap();
    assert!(line.contains("Learn again"), "{line}");
  }

  #[test]
  fn caps_and_new_streams_are_named() {
    let mut log = started();
    let line = log.observe(LevelerSnapshot { gain_x10: 240, speech_ms: 0, ..snap() }).unwrap();
    assert!(line.contains("boost cap"), "{line}");
    assert!(line.contains("new stream"), "{line}");
  }
}
