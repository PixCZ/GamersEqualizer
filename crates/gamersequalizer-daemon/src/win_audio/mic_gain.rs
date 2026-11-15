//! Automatic input gain: moves the Windows mic volume only at the extremes, where the voice
//! leveler can't help. The leveler compensates every volume change exactly (it is told the gain),
//! so others don't hear these steps; they only keep the signal out of clipping and out of the noise.
//!
//! Dead band: as long as the leveler's gain stays inside its range and nothing clips, the volume
//! is left alone, wherever it is. Hysteresis comes from the step size: after +6 dB worth of
//! volume the leveler needs ~6 dB less, far from the edge that triggered it.

use super::chatmix::{get_endpoint_volume_db, ChatMixController};
use gamersequalizer_common::shm::ShmHeader;
use log::info;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Clipped raw samples within `CLIP_WINDOW` that mean the converter is overdriven. Only clips
/// next to speech count (see `AutoInputGain::tick`): a tap on the mic or a desk thump clips too,
/// and a lower volume for those would only make the voice quieter.
const CLIP_LIMIT: u32 = 50;
const CLIP_WINDOW: Duration = Duration::from_secs(10);
/// Leveler pinned near its boost cap (+24) / its cut cap (-6) for this much speech = the input
/// is too weak / too hot for it.
const WEAK_GAIN_DB: f32 = 18.0;
const HOT_GAIN_DB: f32 = -5.0;
const PINNED_SPEECH_MS: u32 = 20_000;
/// Steps of the Windows slider (its dB per step depends on the device; the leveler is told the
/// real dB, so the size doesn't have to be exact).
const STEP_UP: f32 = 0.10;
const STEP_DOWN: f32 = 0.10;
const STEP_DOWN_CLIP: f32 = 0.15;
/// Never beyond these Windows levels on its own.
const MIN_LEVEL: f32 = 0.20;
const MAX_LEVEL: f32 = 1.0;
/// After a step (or after the user/an app moved the volume) wait this long before the next one.
/// Clipping waits less: distortion can't be undone later.
const COOLDOWN: Duration = Duration::from_secs(30);
const COOLDOWN_CLIP: Duration = Duration::from_secs(3);

/// What the daemon sees once per tick (~250 ms).
#[derive(Debug, Clone, Copy)]
pub struct MicGainInput {
  pub now: Instant,
  /// DSP counters (cumulative): clipped raw samples, ms of speech. A new APO instance (audio
  /// restart, resume, replug, an app reopening the mic) starts them at 0 again.
  pub clips: u32,
  pub speech_ms: u32,
  /// Gain the leveler applies now (dB).
  pub leveler_gain_db: f32,
  /// Windows mic volume now (0..1).
  pub level: f32,
  /// Last time someone else (user, app) moved the volume.
  pub changed_at: Option<Instant>,
}

#[derive(Debug, Default)]
pub struct AutoInputGain {
  /// Counters of the previous tick.
  last: Option<(u32, u32)>,
  clip_window: Option<(Instant, u32)>,
  /// Clips of the last tick if it had no speech: they count once the next tick has some.
  loose_clips: u32,
  weak_ms: u32,
  hot_ms: u32,
  stepped_at: Option<Instant>,
  /// Highest level since clipping lowered it: a raise never goes back above it (some converters
  /// misbehave at high gain; log 2026-11-06: raised 78 -> 88 %, clipped 7 s later). Lives as long
  /// as the daemon on this mic; persist it per device if it keeps being relearned.
  ceiling: Option<f32>,
}

impl AutoInputGain {
  /// Forget the counters (automatic gain off). The time of the last step and the clip ceiling
  /// (same device) stay.
  pub fn reset(&mut self) {
    *self = Self { stepped_at: self.stepped_at, ceiling: self.ceiling, ..Self::default() };
  }

  /// The new Windows mic volume, if one is due. Steps happen only in a pause (no speech since the
  /// last tick): the DSP learns the new gain a moment later, which nobody hears between words.
  pub fn tick(&mut self, i: MicGainInput) -> Option<f32> {
    let (clips, speech) = match self.last.replace((i.clips, i.speech_ms)) {
      // Counters went back: a new DSP instance, not ~4e9 clips. Start over from these values.
      // (A real u32 wrap would need 49 days of speech; it just costs one restart too.)
      Some((c, s)) if i.clips < c || i.speech_ms < s => {
        self.reset();
        self.last = Some((i.clips, i.speech_ms));
        return None;
      }
      Some((c, s)) => (i.clips - c, i.speech_ms - s),
      None => return None,
    };
    // Clips count only with speech in the same tick or the next one (the DSP reports speech
    // later than the raw clip, by the detector's and the chain's delay). Known limit: a tap
    // while talking still counts.
    let loose = std::mem::take(&mut self.loose_clips);
    let clips = if speech > 0 {
      clips.saturating_add(loose)
    } else {
      self.loose_clips = clips;
      0
    };
    let (start, sum) = match self.clip_window {
      Some((start, sum)) if i.now.duration_since(start) < CLIP_WINDOW => (start, sum + clips),
      _ => (i.now, clips),
    };
    self.clip_window = Some((start, sum));
    self.weak_ms = if i.leveler_gain_db >= WEAK_GAIN_DB { self.weak_ms + speech } else { 0 };
    self.hot_ms = if i.leveler_gain_db <= HOT_GAIN_DB { self.hot_ms + speech } else { 0 };

    let waited = |d: Duration| [self.stepped_at, i.changed_at].iter().flatten().all(|&t| i.now.duration_since(t) >= d);
    let clipped = sum >= CLIP_LIMIT && waited(COOLDOWN_CLIP);
    let new = if clipped {
      i.level - STEP_DOWN_CLIP
    } else if self.hot_ms >= PINNED_SPEECH_MS && waited(COOLDOWN) {
      i.level - STEP_DOWN
    } else if self.weak_ms >= PINNED_SPEECH_MS && waited(COOLDOWN) {
      (i.level + STEP_UP).min(self.ceiling.unwrap_or(MAX_LEVEL))
    } else {
      return None;
    };
    let new = new.clamp(MIN_LEVEL, MAX_LEVEL);
    if speech > 0 || (new - i.level).abs() < 0.01 {
      return None; // talking right now (try again next tick), or already at the limit
    }
    let ceiling = if clipped { Some(new) } else { self.ceiling };
    *self = Self { last: self.last, stepped_at: Some(i.now), ceiling, ..Self::default() };
    Some(new)
  }
}

/// Daemon side: once per volume sync (~250 ms), after `ChatMixController::sync_from_system`.
#[derive(Debug, Default)]
pub struct MicGainLink {
  auto: AutoInputGain,
  /// Wish (`mic_volume_permille`) seen last tick: the window writes it when the user moves its slider.
  wish_seen: Option<u32>,
}

impl MicGainLink {
  /// Publishes the input gain (dB) for the leveler and takes an automatic step when one is due.
  /// Returns true when the automatic level changed (the caller saves the settings).
  pub fn tick(&mut self, h: &ShmHeader, chatmix: &mut ChatMixController, mic_id: Option<&str>) -> bool {
    let r = Ordering::Relaxed;
    let wish = h.mic_volume_permille.load(r);
    if self.wish_seen.is_some_and(|w| w != wish) {
      chatmix.mic_changed_at = Some(Instant::now());
    }
    self.wish_seen = Some(wish);
    let before = chatmix.mic_hw_auto;
    let Some((id, (db, hardware))) = mic_id.and_then(|id| Some((id, get_endpoint_volume_db(id)?))) else {
      return false;
    };
    h.mic_input_gain_db_x10.store((db * 10.0).round() as i32, r);

    // Only a volume the device applies itself helps against clipping or noise; a software volume
    // just scales after the converter (the leveler does that already).
    let on = hardware && h.mic_leveler_enabled.load(r) && !h.master_bypass.load(r) && !chatmix.mic_muted;
    if on {
      let input = MicGainInput {
        now: Instant::now(),
        clips: h.mic_input_clips.load(r),
        speech_ms: h.mic_speech_ms.load(r),
        leveler_gain_db: h.mic_leveler_gain_db_x10.load(r) as f32 / 10.0,
        level: chatmix.mic_hw_auto.unwrap_or(chatmix.mic_volume.min(1.0)),
        changed_at: chatmix.mic_changed_at,
      };
      if let Some(level) = self.auto.tick(input) {
        info!(
          "[MicGain] Windows mic volume {:.0} % -> {:.0} % (leveler {:+.1} dB)",
          input.level * 100.0,
          level * 100.0,
          input.leveler_gain_db
        );
        chatmix.set_mic_hw_auto(id, level);
      }
    } else {
      self.auto.reset();
      if chatmix.mic_hw_auto.is_some() {
        info!("[MicGain] off: Windows mic volume back to {:.0} %", chatmix.mic_volume.min(1.0) * 100.0);
        chatmix.clear_mic_hw_auto(id);
      }
    }
    let auto = chatmix.mic_hw_auto.map_or(0, |l| (l * 1000.0).round() as u32);
    h.mic_auto_gain_permille.store(auto, r);
    chatmix.mic_hw_auto != before
  }

  /// The mic changes: its old device gets the wished volume back first.
  pub fn switch_mic(&mut self, chatmix: &mut ChatMixController, old_id: Option<&str>) {
    if let Some(id) = old_id {
      chatmix.clear_mic_hw_auto(id);
    }
    chatmix.mic_hw_auto = None;
    self.auto = AutoInputGain::default(); // another converter: its own clip ceiling
  }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
  use super::*;

  const TICK: Duration = Duration::from_millis(250);

  /// Feeds ticks; `f(tick)` gives (clips this tick, speech ms this tick, leveler gain). Returns
  /// the steps taken as (tick, new level), applying them.
  fn drive(ticks: u32, mut level: f32, f: impl Fn(u32) -> (u32, u32, f32)) -> Vec<(u32, f32)> {
    let mut g = AutoInputGain::default();
    let t0 = Instant::now();
    let (mut clips, mut speech) = (0u32, 0u32);
    let mut steps = vec![];
    for n in 0..ticks {
      let (c, s, gain) = f(n);
      clips = clips.wrapping_add(c);
      speech = speech.wrapping_add(s);
      let input =
        MicGainInput { now: t0 + TICK * n, clips, speech_ms: speech, leveler_gain_db: gain, level, changed_at: None };
      if let Some(new) = g.tick(input) {
        level = new;
        steps.push((n, new));
      }
    }
    steps
  }

  /// Talks 3 ticks, pauses 1 (a pause every second).
  fn talking(n: u32) -> u32 {
    if n % 4 == 3 {
      0
    } else {
      250
    }
  }

  #[test]
  fn counters_restarting_from_zero_are_not_a_clip_burst() {
    let mut g = AutoInputGain::default();
    let t0 = Instant::now();
    let input = |n: u32, clips: u32, speech_ms: u32| MicGainInput {
      now: t0 + TICK * n,
      clips,
      speech_ms,
      leveler_gain_db: 0.0,
      level: 0.8,
      changed_at: None,
    };
    assert_eq!(g.tick(input(0, 120, 90_000)), None);
    // New APO instance after resume: counters back near 0, then a pause (no speech).
    for n in 1..40 {
      assert_eq!(g.tick(input(n, 3, 0)), None, "tick {n}");
    }
  }

  #[test]
  fn inside_the_dead_band_nothing_moves() {
    // A quiet-ish voice the leveler lifts by +12 dB, a few clicks: for 10 minutes, nothing.
    let steps = drive(2400, 0.5, |n| (u32::from(n % 40 == 0), talking(n), 12.0));
    assert!(steps.is_empty(), "{steps:?}");
  }

  #[test]
  fn clipping_lowers_the_volume_quickly_but_only_in_a_pause() {
    let steps = drive(40, 0.9, |n| (if talking(n) > 0 { 30 } else { 0 }, talking(n), 0.0));
    let (tick, level) = steps[0];
    assert!(tick < 8, "within ~2 s: tick {tick}");
    assert_eq!(talking(tick), 0, "in a pause");
    assert!((level - 0.75).abs() < 1e-6);
  }

  #[test]
  fn taps_on_the_mic_without_speech_do_not_lower_the_volume() {
    // A hard tap every 2 s for 2 minutes, nobody talks.
    let steps = drive(480, 0.9, |n| (if n % 8 == 0 { 200 } else { 0 }, 0, 0.0));
    assert!(steps.is_empty(), "{steps:?}");
    // A clipped word whose speech flag arrives one tick after its clips still counts.
    let steps = drive(40, 0.9, |n| (if n == 5 { 200 } else { 0 }, if (6..9).contains(&n) { 250 } else { 0 }, 0.0));
    assert_eq!(steps.len(), 1, "{steps:?}");
  }

  #[test]
  fn a_weak_input_is_raised_after_20_s_of_speech_and_then_waits() {
    let steps = drive(400, 0.4, |n| (0, talking(n), 22.0)); // leveler near its +24 cap
                                                            // 20 s of speech at 750 ms per second -> ~27 s, then one step every 30 s at most.
    assert!(steps.len() >= 2, "{steps:?}");
    assert!(steps[0].0 >= 100 && steps[0].0 <= 120, "{steps:?}");
    assert!(steps[1].0 - steps[0].0 >= 120, "cooldown: {steps:?}");
    assert!((steps[0].1 - 0.5).abs() < 1e-6);
  }

  #[test]
  fn after_clipping_a_weak_input_is_never_raised_back_to_where_it_clipped() {
    // Clips at 0.88 for the first 10 s, then the leveler reads "weak" for 5 minutes.
    let steps = drive(1200, 0.88, |n| {
      let s = talking(n);
      if n < 40 {
        (if s > 0 { 30 } else { 0 }, s, 24.0)
      } else {
        (0, s, 24.0)
      }
    });
    let (_, ceiling) = steps[0];
    assert!((ceiling - 0.73).abs() < 1e-6, "{steps:?}");
    assert!(steps.iter().all(|&(_, l)| l <= ceiling + 1e-6), "{steps:?}");
  }

  #[test]
  fn limits_are_kept() {
    let up = drive(4000, 0.95, |n| (0, talking(n), 22.0));
    assert_eq!(up.last().map(|s| s.1), Some(MAX_LEVEL));
    let down = drive(4000, 0.3, |n| (100, talking(n), 0.0));
    assert_eq!(down.last().map(|s| s.1), Some(MIN_LEVEL));
  }

  #[test]
  fn someone_elses_change_is_respected_for_a_while() {
    let mut g = AutoInputGain::default();
    let t0 = Instant::now();
    let input = |n: u32, speech| MicGainInput {
      now: t0 + TICK * n,
      clips: 0,
      speech_ms: speech,
      leveler_gain_db: 22.0,
      level: 0.4,
      changed_at: Some(t0 + TICK * 100),
    };
    let mut speech = 0;
    for n in 0..200 {
      speech += talking(n);
      assert_eq!(g.tick(input(n, speech)), None, "tick {n}: the user just set this volume");
    }
  }
}
