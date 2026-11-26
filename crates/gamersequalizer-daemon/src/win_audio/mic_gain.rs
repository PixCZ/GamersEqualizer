//! Automatic input gain: moves the Windows mic volume only at the extremes, where the voice
//! leveler can't help. The leveler compensates every volume change exactly (it is told the gain),
//! so others don't hear these steps; they only keep the signal out of clipping and out of the noise.
//!
//! It decides by the voice level at the converter (the leveler's learned level plus the Windows
//! volume), not by the leveler's gain: that gain also holds the user's target loudness, so a low
//! target read a normal voice as too hot and pulled the volume down towards the noise.
//!
//! One exception goes by the gain: a leveler pinned at its boost cap can't reach a high target, so
//! that counts as weak too (a higher volume never makes the noise worse against the voice).
//!
//! Dead band: as long as the voice stays between `WEAK_VOICE_DB` and `HOT_VOICE_DB` and nothing
//! clips, the volume is left alone, wherever it is. Hysteresis comes from the step size: after
//! +6 dB worth of volume the voice is ~6 dB louder, far from the edge that triggered it.

use super::chatmix::{ChatMixController, get_endpoint_volume_db};
use gamersequalizer_common::shm::ShmHeader;
use gamersequalizer_mic_chain::voice_leveler::MAX_BOOST_DB;
use log::info;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Clipped raw samples within `CLIP_WINDOW` that mean the converter is overdriven. Only clips
/// next to speech count (see `AutoInputGain::tick`): a tap on the mic or a desk thump clips too,
/// and a lower volume for those would only make the voice quieter.
const CLIP_LIMIT: u32 = 50;
const CLIP_WINDOW: Duration = Duration::from_secs(10);
/// Voice (active speech level, dBFS) at the converter this low / this high for this much speech =
/// the input is too weak / too hot. They equal the leveler at +18 / -5 dB with the default target
/// (-24 dBFS), where the rules were tuned. The level is learned after the echo canceller, denoiser
/// and mic EQ, so it is a few dB off the raw converter; clipping is counted on the raw input.
const WEAK_VOICE_DB: f32 = -42.0;
const HOT_VOICE_DB: f32 = -19.0;
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
/// Someone moving the volume at least halfway back within `REVERT_WINDOW` of our step set it back:
/// an app that holds the mic volume. After `REVERTS_TO_STOP` of those the automatic gain stops on
/// this mic (until another mic or a service restart) instead of stepping against it forever. Two
/// in a row, not one: a user moving the slider right after a step is a coincidence once; a step
/// that stands or any other change starts the count again.
const REVERT_WINDOW: Duration = Duration::from_secs(10);
const REVERTS_TO_STOP: u8 = 2;

/// What the daemon sees once per tick (~250 ms).
#[derive(Debug, Clone, Copy)]
pub struct MicGainInput {
  pub now: Instant,
  /// DSP counters (cumulative): clipped raw samples, ms of speech. A new APO instance (audio
  /// restart, resume, replug, an app reopening the mic) starts them at 0 again.
  pub clips: u32,
  pub speech_ms: u32,
  /// Voice level at the converter (dBFS), once the leveler has learned one.
  pub voice_db: Option<f32>,
  /// The leveler gives all the boost it has and is still short of the target.
  pub boost_capped: bool,
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
  /// The last step as (when, level before, level after) until someone else moves the volume.
  last_step: Option<(Instant, f32, f32)>,
  /// Steps someone set back right away (`REVERT_WINDOW`).
  reverts: u8,
}

impl AutoInputGain {
  /// Forget the counters (automatic gain off). The last step, the clip ceiling and the reverts
  /// (same device) stay.
  pub fn reset(&mut self) {
    *self = self.kept();
  }

  /// Someone kept setting our steps back: no more steps on this mic.
  pub fn stopped(&self) -> bool {
    self.reverts >= REVERTS_TO_STOP
  }

  /// The new Windows mic volume, if one is due. Steps happen only in a pause (no speech since the
  /// last tick): the DSP learns the new gain a moment later, which nobody hears between words.
  pub fn tick(&mut self, i: MicGainInput) -> Option<f32> {
    if let (Some((at, before, after)), Some(changed)) = (self.last_step, i.changed_at)
      && changed > at
    {
      self.last_step = None;
      // Halfway at least: a device rounding our level reads as someone else's change too.
      let back = changed.duration_since(at) <= REVERT_WINDOW && (i.level - after) / (before - after) >= 0.5;
      self.reverts = if back { self.reverts + 1 } else { 0 };
    }
    if self.stopped() {
      return None;
    }
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
    let weak = i.boost_capped || i.voice_db.is_some_and(|v| v <= WEAK_VOICE_DB);
    self.weak_ms = if weak { self.weak_ms + speech } else { 0 };
    self.hot_ms = if i.voice_db.is_some_and(|v| v >= HOT_VOICE_DB) { self.hot_ms + speech } else { 0 };

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
    *self = Self {
      last: self.last,
      stepped_at: Some(i.now),
      ceiling,
      last_step: Some((i.now, i.level, new)),
      // The previous step still stands (nobody touched it): no revert in a row.
      reverts: if self.last_step.is_some() { 0 } else { self.reverts },
      ..self.kept()
    };
    Some(new)
  }

  /// What outlives `reset` and a step; everything else back to its default.
  fn kept(&self) -> Self {
    Self {
      stepped_at: self.stepped_at,
      ceiling: self.ceiling,
      last_step: self.last_step,
      reverts: self.reverts,
      ..Self::default()
    }
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
    let on = hardware && h.mic_leveler_enabled.load(r) && !h.master_bypass.load(r) && !chatmix.mic.muted;
    if on {
      // Learned without the Windows volume; 0 = nothing learned yet.
      let level_x10 = h.mic_leveler_level_db_x10.load(r);
      let input = MicGainInput {
        now: Instant::now(),
        clips: h.mic_input_clips.load(r),
        speech_ms: h.mic_speech_ms.load(r),
        voice_db: (level_x10 != 0).then(|| level_x10 as f32 / 10.0 + db),
        // Within 1 dB: the applied gain ramps towards the cap and is published rounded.
        boost_capped: h.mic_leveler_gain_db_x10.load(r) as f32 / 10.0 >= MAX_BOOST_DB - 1.0,
        level: chatmix.mic_hw_auto.unwrap_or(chatmix.mic.volume.min(1.0)),
        changed_at: chatmix.mic_changed_at,
      };
      let stopped = self.auto.stopped();
      let step = self.auto.tick(input);
      if !stopped && self.auto.stopped() {
        info!(
          "[MicGain] stopped on this mic: someone (an app holding the mic volume?) set it back {REVERTS_TO_STOP}x within {} s of a step; it stays where they set it until another mic or a restart",
          REVERT_WINDOW.as_secs()
        );
      }
      if let Some(level) = step {
        let voice = input.voice_db.map_or("not learned yet".to_string(), |v| format!("{v:.1} dBFS"));
        info!("[MicGain] Windows mic volume {:.0} % -> {:.0} % (voice {voice})", input.level * 100.0, level * 100.0);
        chatmix.set_mic_hw_auto(id, level);
      }
    } else {
      self.auto.reset();
      if chatmix.mic_hw_auto.is_some() {
        info!("[MicGain] off: Windows mic volume back to {:.0} %", chatmix.mic.volume.min(1.0) * 100.0);
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
mod tests {
  use super::*;

  const TICK: Duration = Duration::from_millis(250);

  /// Feeds ticks; `f(tick)` gives (clips this tick, speech ms this tick, voice dBFS). Returns
  /// the steps taken as (tick, new level), applying them.
  fn drive(ticks: u32, mut level: f32, f: impl Fn(u32) -> (u32, u32, Option<f32>)) -> Vec<(u32, f32)> {
    let mut g = AutoInputGain::default();
    let t0 = Instant::now();
    let (mut clips, mut speech) = (0u32, 0u32);
    let mut steps = vec![];
    for n in 0..ticks {
      let (c, s, voice_db) = f(n);
      clips = clips.wrapping_add(c);
      speech = speech.wrapping_add(s);
      let input = MicGainInput {
        now: t0 + TICK * n,
        clips,
        speech_ms: speech,
        voice_db,
        boost_capped: false,
        level,
        changed_at: None,
      };
      if let Some(new) = g.tick(input) {
        level = new;
        steps.push((n, new));
      }
    }
    steps
  }

  /// Talks 3 ticks, pauses 1 (a pause every second).
  fn talking(n: u32) -> u32 {
    if n % 4 == 3 { 0 } else { 250 }
  }

  #[test]
  fn counters_restarting_from_zero_are_not_a_clip_burst() {
    let mut g = AutoInputGain::default();
    let t0 = Instant::now();
    let input = |n: u32, clips: u32, speech_ms: u32| MicGainInput {
      now: t0 + TICK * n,
      clips,
      speech_ms,
      voice_db: Some(-24.0),
      boost_capped: false,
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
    // A quiet-ish voice (the leveler lifts it by +12 dB to the default target), a few clicks: for
    // 10 minutes, nothing.
    let steps = drive(2400, 0.5, |n| (u32::from(n % 40 == 0), talking(n), Some(-36.0)));
    assert!(steps.is_empty(), "{steps:?}");
  }

  #[test]
  fn clipping_lowers_the_volume_quickly_but_only_in_a_pause() {
    let steps = drive(40, 0.9, |n| (if talking(n) > 0 { 30 } else { 0 }, talking(n), Some(-24.0)));
    let (tick, level) = steps[0];
    assert!(tick < 8, "within ~2 s: tick {tick}");
    assert_eq!(talking(tick), 0, "in a pause");
    assert!((level - 0.75).abs() < 1e-6);
  }

  #[test]
  fn taps_on_the_mic_without_speech_do_not_lower_the_volume() {
    // A hard tap every 2 s for 2 minutes, nobody talks.
    let steps = drive(480, 0.9, |n| (if n % 8 == 0 { 200 } else { 0 }, 0, Some(-24.0)));
    assert!(steps.is_empty(), "{steps:?}");
    // A clipped word whose speech flag arrives one tick after its clips still counts.
    let steps =
      drive(40, 0.9, |n| (if n == 5 { 200 } else { 0 }, if (6..9).contains(&n) { 250 } else { 0 }, Some(-24.0)));
    assert_eq!(steps.len(), 1, "{steps:?}");
  }

  #[test]
  fn a_weak_input_is_raised_after_20_s_of_speech_and_then_waits() {
    let steps = drive(400, 0.4, |n| (0, talking(n), Some(-46.0))); // 4 dB under the weak line
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
      if n < 40 { (if s > 0 { 30 } else { 0 }, s, Some(-48.0)) } else { (0, s, Some(-48.0)) }
    });
    let (_, ceiling) = steps[0];
    assert!((ceiling - 0.73).abs() < 1e-6, "{steps:?}");
    assert!(steps.iter().all(|&(_, l)| l <= ceiling + 1e-6), "{steps:?}");
  }

  #[test]
  fn limits_are_kept() {
    let up = drive(4000, 0.95, |n| (0, talking(n), Some(-46.0)));
    assert_eq!(up.last().map(|s| s.1), Some(MAX_LEVEL));
    let down = drive(4000, 0.3, |n| (100, talking(n), Some(-24.0)));
    assert_eq!(down.last().map(|s| s.1), Some(MIN_LEVEL));
  }

  #[test]
  fn a_leveler_at_its_boost_cap_raises_the_volume_whatever_the_voice() {
    // Target -12, voice -38: the leveler needs +26 and has +24.
    let mut g = AutoInputGain::default();
    let t0 = Instant::now();
    let (mut speech, mut level, mut steps) = (0, 0.5, 0);
    for n in 0..200 {
      speech += talking(n);
      let input = MicGainInput {
        now: t0 + TICK * n,
        clips: 0,
        speech_ms: speech,
        voice_db: Some(-38.0),
        boost_capped: true,
        level,
        changed_at: None,
      };
      if let Some(new) = g.tick(input) {
        (level, steps) = (new, steps + 1);
      }
    }
    assert_eq!(steps, 1);
    assert!((level - 0.6).abs() < 1e-6);
  }

  #[test]
  fn a_hot_voice_is_lowered_after_20_s_of_speech() {
    let steps = drive(200, 0.9, |n| (0, talking(n), Some(-15.0)));
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert!((steps[0].1 - 0.8).abs() < 1e-6);
  }

  #[test]
  fn a_normal_voice_stays_whatever_the_target_and_so_does_an_unlearned_one() {
    // -31 dBFS: with the target at -36 the leveler cuts 5 dB, which used to count as too hot.
    assert!(drive(2400, 0.8, |n| (0, talking(n), Some(-31.0))).is_empty());
    assert!(drive(2400, 0.8, |n| (0, talking(n), None)).is_empty());
  }

  /// Clipping speech from 100 %; `delay` ticks after each of our steps an app sets `app(level)`.
  /// Returns our steps and whether the automatic gain stopped.
  fn with_app(ticks: u32, delay: u32, app: impl Fn(f32) -> f32) -> (Vec<f32>, bool) {
    let mut g = AutoInputGain::default();
    let t0 = Instant::now();
    let (mut clips, mut speech, mut level) = (0, 0, 1.0);
    let (mut changed_at, mut app_due, mut steps) = (None, None, vec![]);
    for n in 0..ticks {
      let now = t0 + TICK * n;
      if app_due == Some(n) {
        (level, changed_at) = (app(level), Some(now));
      }
      speech += talking(n);
      clips += if talking(n) > 0 { 30 } else { 0 };
      let input =
        MicGainInput { now, clips, speech_ms: speech, voice_db: Some(-24.0), boost_capped: false, level, changed_at };
      if let Some(new) = g.tick(input) {
        (level, app_due) = (new, Some(n + delay));
        steps.push(new);
      }
    }
    (steps, g.stopped())
  }

  #[test]
  fn an_app_setting_the_volume_back_stops_the_automatic_gain() {
    let (steps, stopped) = with_app(2400, 2, |_| 1.0);
    assert_eq!(steps.len(), 2, "{steps:?}");
    assert!(stopped);
    // An app's own control moving on the same way is no revert: down to the floor.
    let (steps, stopped) = with_app(2400, 2, |l| l - 0.05);
    assert!(steps.len() > 2, "{steps:?}");
    assert!(!stopped);
    // Set back only after 12.5 s: someone's own change, not a revert.
    let (steps, stopped) = with_app(2400, 50, |_| 1.0);
    assert!(steps.len() > 2, "{steps:?}");
    assert!(!stopped);
  }

  #[test]
  fn someone_elses_change_is_respected_for_a_while() {
    let mut g = AutoInputGain::default();
    let t0 = Instant::now();
    let input = |n: u32, speech| MicGainInput {
      now: t0 + TICK * n,
      clips: 0,
      speech_ms: speech,
      voice_db: Some(-46.0),
      boost_capped: false,
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
