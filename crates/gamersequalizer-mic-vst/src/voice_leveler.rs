//! Learning voice leveler: brings speech to one level whatever the mic gain or how loud the user
//! talks today. It learns the speech level only from frames the voice detector heard as speech
//! (never from noise, keys or whisper, which the detector doesn't hear), fast at first and then
//! ever slower: a running mean (rate 1/n) until it has `MEMORY_FRAMES` of speech, then a fixed
//! rate, i.e. it remembers roughly the last 30 s of speech. The gain only moves while it learns,
//! so it never pumps up the noise in pauses (that is not a compressor).
//!
//! The level is learned without the input gain (Windows mic volume, `set_input_gain_db`): when
//! the volume moves, the gain compensates at once instead of relearning.

use std::collections::VecDeque;

/// 10 ms at 48 kHz.
const FRAME: usize = 480;
/// Default active speech level the output is brought to (power mean over speech frames), dBFS.
/// ITU-T P.56's reference is -26 dBov; voice chat apps expect a bit hotter.
pub const DEFAULT_TARGET_DB: f32 = -24.0;
/// Range of the user's "target loudness" setting.
pub const MIN_TARGET_DB: f32 = -36.0;
pub const MAX_TARGET_DB: f32 = -12.0;
/// A level remembered from an earlier stream counts like this many speech frames (1 s): the first
/// sentences of a new call can still move it quickly (other room, other mood), then it slows again.
const WARM_START_FRAMES: u32 = 100;
/// Gain range. +24 dB lifts a quiet voice on a mic set 12 dB low (private recording: own voice
/// ~-31 dBFS active level, so -12 dB needs ~+19); room noise between words stays under -80 dBFS
/// with the gate on. The cut tames a hot mic before the limiter.
const MAX_BOOST_DB: f32 = 24.0;
const MAX_CUT_DB: f32 = -6.0;
/// Speech frames before any gain is applied: one syllable is no level yet.
const MIN_FRAMES: u32 = 30;
/// Running mean up to this many speech frames (30 s), then this fixed rate.
const MEMORY_FRAMES: u32 = 3000;
/// After `SETTLED_FRAMES` of speech (3 s), one frame moves the estimate as if it were at most this
/// far from it (a shout, a cough or a desk thump counts as a bit louder speech, not as a new
/// level). Not earlier: a first estimate from a few soft syllables must be able to jump up.
const OUTLIER_DB: f32 = 6.0;
const SETTLED_FRAMES: u32 = 300;
/// After `SETTLED_FRAMES`, a "speech" frame this far under the learned level is not the user's
/// own voice (friends or game voices leaking into the mic, echo, a word the denoiser removed) and
/// teaches nothing. Clamping it like a loud outlier was not enough: the clamp is relative to the
/// estimate, so minutes of such frames walked it down without limit (log 2026-11-06: leveler
/// pinned at +24 dB while the raw mic clipped, which the limiter then crushed).
const FOREIGN_DB: f32 = 12.0;
/// Per-sample smoothing of the applied gain (~100 ms), so the fast first second doesn't zip.
const GAIN_COEF: f32 = 1.0 / (48.0 * 100.0);
/// Bound on voice flags waiting for their audio to reach the leveler. They expire by time
/// (`voice_at`); 64 was too few for small host blocks (1 ms blocks x ~70 ms lead dropped flags
/// still due, so speech frames counted as non-speech).
const QUEUE_CAPACITY: usize = 1024;

pub struct VoiceLeveler {
  /// Learned speech level without the input gain, linear power; valid once `frames > 0`.
  level_pow: f32,
  /// Speech frames learned so far (saturates at `MEMORY_FRAMES`).
  frames: u32,
  /// Gain currently applied (linear) and the one it moves to.
  gain: f32,
  target_gain: f32,
  target_db: f32,
  /// Output samples processed so far.
  pos: u64,
  /// (start, end) in output samples of a block the detector heard, and whether it was speech.
  pending: VecDeque<(u64, u64, bool)>,
  acc: f32,
  acc_len: usize,
  /// Seeded or learned in this stream: a remembered level from an earlier stream no longer applies.
  started: bool,
  /// Gain in front of the chain (dB) and its power factor.
  input_gain_db: f32,
  input_pow: f32,
  /// Speech frames learned from, all time (wraps).
  speech_frames: u32,
}

impl Default for VoiceLeveler {
  fn default() -> Self {
    Self::new()
  }
}

impl VoiceLeveler {
  pub fn new() -> Self {
    Self {
      level_pow: 0.0,
      frames: 0,
      gain: 1.0,
      target_gain: 1.0,
      target_db: DEFAULT_TARGET_DB,
      pos: 0,
      pending: VecDeque::with_capacity(QUEUE_CAPACITY),
      acc: 0.0,
      acc_len: 0,
      started: false,
      input_gain_db: 0.0,
      input_pow: 1.0,
      speech_frames: 0,
    }
  }

  /// Forget the learned level (leveler switched off, or a new voice).
  /// No allocation: called from the audio thread.
  pub fn reset(&mut self) {
    self.level_pow = 0.0;
    self.frames = 0;
    self.gain = 1.0;
    self.target_gain = 1.0;
    self.pos = 0; // target_db is a setting: kept
    self.pending.clear();
    self.acc = 0.0;
    self.acc_len = 0;
    self.started = false; // input gain is a setting: kept
  }

  /// Something before the leveler changed the speech level (mic volume, denoiser, EQ, ...): learn
  /// fast again. The learned level and the gain stay; the level only counts as `MIN_FRAMES` (0.3 s)
  /// of speech, so the rate restarts at 1/n and the outlier clamp is off until it settles again.
  /// No allocation: audio thread.
  pub fn relearn(&mut self) {
    // Fewer frames: already learning fast (and below MIN_FRAMES there is no level to keep).
    self.frames = self.frames.min(MIN_FRAMES);
  }

  /// Applied gain in dB (0 until it has heard enough speech).
  pub fn gain_db(&self) -> f32 {
    20.0 * self.gain.max(1e-6).log10()
  }

  /// Learned speech level without the input gain (dBFS), once there is one worth remembering.
  pub fn level_db(&self) -> Option<f32> {
    // `max` hides a NaN as 1e-12 (-120 dB), which the service would save as the mic's level.
    (self.frames >= MIN_FRAMES && self.level_pow.is_finite()).then(|| 10.0 * self.level_pow.max(1e-12).log10())
  }

  /// Speech heard so far (ms, wraps): how long the user talked, for the automatic input gain.
  pub fn speech_ms(&self) -> u32 {
    self.speech_frames.wrapping_mul(10)
  }

  /// Gain applied to the mic before this chain (Windows mic volume), dB. The learned level stays;
  /// the applied gain follows at once (within the ~100 ms smoothing).
  pub fn set_input_gain_db(&mut self, gain_db: f32) {
    // Windows mic volumes span about -96..+30 dB; anything beyond is garbage from the mapping.
    let gain_db = gain_db.clamp(-60.0, 60.0);
    if gain_db.is_finite() && gain_db != self.input_gain_db {
      self.input_gain_db = gain_db;
      self.input_pow = 10f32.powf(gain_db / 10.0);
      self.update_target_gain();
    }
  }

  /// Output loudness the user wants (clamped to the setting's range).
  pub fn set_target_db(&mut self, target_db: f32) {
    let t = target_db.clamp(MIN_TARGET_DB, MAX_TARGET_DB);
    if t != self.target_db {
      self.target_db = t;
      self.update_target_gain();
    }
  }

  /// Start from a level learned in an earlier stream (same mic): the first word already has the
  /// right gain, and learning stays fast for the first sentences. Ignored once it learned here.
  pub fn seed(&mut self, level_db: f32) {
    if self.started || !level_db.is_finite() {
      return;
    }
    self.started = true;
    self.level_pow = 10f32.powf(level_db / 10.0);
    self.frames = WARM_START_FRAMES;
    self.update_target_gain();
    self.gain = self.target_gain;
  }

  /// Levels `samples` in place. `voice`: the detector heard speech in the block it just processed;
  /// `lead`: how many samples that audio reaches this point later (denoiser latency + lookahead).
  pub fn process(&mut self, samples: &mut [f32], voice: bool, lead: usize) {
    let len = samples.len() as u64;
    // Expired: played, and before the frame in progress (whose middle `voice_at` asks about).
    while self.pending.front().is_some_and(|&(_, end, _)| end + FRAME as u64 <= self.pos) {
      self.pending.pop_front();
    }
    if self.pending.len() >= QUEUE_CAPACITY {
      self.pending.pop_front();
    }
    let start = self.pos + lead as u64;
    self.pending.push_back((start, start + len, voice));

    for s in samples.iter_mut() {
      self.acc += *s * *s;
      self.acc_len += 1;
      if self.acc_len == FRAME {
        // This frame's middle, in output samples: was the audio there speech?
        let mid = self.pos + 1 - (FRAME / 2) as u64;
        let voiced = self.voice_at(mid);
        self.learn(self.acc / FRAME as f32, voiced);
        self.acc = 0.0;
        self.acc_len = 0;
      }
      self.pos += 1;
      self.gain += (self.target_gain - self.gain) * GAIN_COEF;
      *s *= self.gain;
    }
  }

  fn voice_at(&mut self, t: u64) -> bool {
    while self.pending.front().is_some_and(|&(_, end, _)| end <= t) {
      self.pending.pop_front();
    }
    self.pending.front().is_some_and(|&(start, _, voice)| voice && start <= t)
  }

  fn learn(&mut self, pow: f32, voice: bool) {
    // `!(pow > 1e-12)` also rejects NaN, which `pow <= 1e-12` let through.
    if !voice || !(pow > 1e-12 && pow.is_finite()) {
      return;
    }
    let pow = pow / self.input_pow;
    let pow = if self.frames >= SETTLED_FRAMES {
      if pow < self.level_pow / 10f32.powf(FOREIGN_DB / 10.0) {
        return;
      }
      let limit = 10f32.powf(OUTLIER_DB / 10.0);
      pow.clamp(self.level_pow / limit, self.level_pow * limit)
    } else {
      pow
    };
    self.speech_frames = self.speech_frames.wrapping_add(1);
    self.started = true;
    self.frames = (self.frames + 1).min(MEMORY_FRAMES);
    self.level_pow += (pow - self.level_pow) / self.frames as f32;
    self.update_target_gain();
  }

  fn update_target_gain(&mut self) {
    if let Some(level) = self.level_db() {
      let gain_db = (self.target_db - level - self.input_gain_db).clamp(MAX_CUT_DB, MAX_BOOST_DB);
      self.target_gain = 10f32.powf(gain_db / 20.0);
    }
  }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
  use super::*;

  const SR: usize = 48_000;

  /// Speech stand-in: a 200 Hz tone at `rms_db`, flagged as voice.
  fn tone(secs: f32, rms_db: f32) -> Vec<f32> {
    let amp = 10f32.powf(rms_db / 20.0) * std::f32::consts::SQRT_2;
    (0..(secs * SR as f32) as usize).map(|i| amp * (i as f32 * 200.0 * std::f32::consts::TAU / SR as f32).sin()).collect()
  }

  fn run(lv: &mut VoiceLeveler, x: &[f32], voice: bool) -> Vec<f32> {
    let mut out = x.to_vec();
    out.chunks_mut(FRAME).for_each(|b| lv.process(b, voice, 0));
    out
  }

  fn rms_db(x: &[f32]) -> f32 {
    10.0 * (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).max(1e-12).log10()
  }

  #[test]
  fn quiet_voice_reaches_the_target_within_a_second_or_two() {
    let mut lv = VoiceLeveler::new();
    let out = run(&mut lv, &tone(3.0, -38.0), true);
    let last = &out[2 * SR..];
    assert!((rms_db(last) - DEFAULT_TARGET_DB).abs() < 0.5, "{:.1} dBFS", rms_db(last));
  }

  /// A NaN frame must not become the learned level (it read as -120 dB, +24 dB gain, and the
  /// service saved it for every later stream).
  #[test]
  fn a_nan_frame_teaches_nothing() {
    let mut lv = VoiceLeveler::new();
    run(&mut lv, &tone(2.0, -30.0), true);
    let learned = lv.level_db();
    let mut bad = vec![f32::NAN; FRAME];
    lv.process(&mut bad, true, 0);
    assert_eq!(lv.level_db(), learned);
    lv.set_input_gain_db(f32::INFINITY);
    assert!(lv.gain_db().is_finite());
  }

  /// Regression: 1 ms host blocks with a 70 ms denoiser lead. The 64-entry queue dropped every
  /// voice flag before its audio arrived, so the leveler never learned.
  #[test]
  fn small_blocks_with_a_long_lead_still_learn() {
    let mut lv = VoiceLeveler::new();
    let mut x = tone(3.0, -38.0);
    x.chunks_mut(SR / 1000).for_each(|b| lv.process(b, true, 70 * SR / 1000));
    assert!(lv.level_db().is_some(), "nothing learned");
    assert!((rms_db(&x[2 * SR..]) - DEFAULT_TARGET_DB).abs() < 1.0, "{:.1} dBFS", rms_db(&x[2 * SR..]));
  }

  #[test]
  fn noise_without_voice_teaches_nothing() {
    let mut lv = VoiceLeveler::new();
    let out = run(&mut lv, &tone(3.0, -50.0), false);
    assert_eq!(lv.gain_db(), 0.0);
    assert!((rms_db(&out) + 50.0).abs() < 0.1);
  }

  #[test]
  fn boost_is_capped() {
    let mut lv = VoiceLeveler::new();
    run(&mut lv, &tone(3.0, -70.0), true);
    assert!((lv.gain_db() - MAX_BOOST_DB).abs() < 0.1, "{:.1} dB", lv.gain_db());
  }

  #[test]
  fn a_shout_after_learning_barely_moves_the_gain() {
    let mut lv = VoiceLeveler::new();
    run(&mut lv, &tone(10.0, -36.0), true);
    let before = lv.gain_db();
    run(&mut lv, &tone(0.5, -6.0), true);
    let after = lv.gain_db();
    assert!(before - after < 1.5, "{before:.1} -> {after:.1} dB");
  }

  #[test]
  fn a_softer_day_is_followed_slowly() {
    let mut lv = VoiceLeveler::new();
    run(&mut lv, &tone(60.0, -30.0), true);
    let normal = lv.gain_db();
    run(&mut lv, &tone(60.0, -40.0), true);
    // 10 dB quieter for a minute of speech: well on the way, not instantly. (A power mean
    // follows a quieter voice slower than a louder one.)
    let quieter = lv.gain_db();
    assert!(quieter - normal > 5.0 && quieter - normal < 10.5, "{normal:.1} -> {quieter:.1} dB");
  }

  #[test]
  fn voice_flags_are_applied_after_the_lead() {
    // The detector hears speech `lead` early: audio still on its way (silence) must not be learned.
    let lead = 4800;
    let mut lv = VoiceLeveler::new();
    let mut x = vec![1e-4f32; SR]; // -80 dBFS hiss, flagged as voice only through the lead
    x.extend(tone(2.0, -36.0));
    let mut out = x.clone();
    let mut pos = 0;
    for b in out.chunks_mut(FRAME) {
      let ahead = pos + lead; // the detector hears this far ahead
      lv.process(b, ahead >= SR, lead);
      pos += FRAME;
    }
    let last = &out[2 * SR..];
    assert!((rms_db(last) - DEFAULT_TARGET_DB).abs() < 0.5, "{:.1} dBFS", rms_db(last));
  }

  #[test]
  fn quiet_foreign_voices_in_long_pauses_dont_walk_the_level_down() {
    // Own voice at -30 dBFS, then 5 min of friends/game voices leaking in at -50, flagged as voice.
    let mut lv = VoiceLeveler::new();
    run(&mut lv, &tone(10.0, -30.0), true);
    let before = lv.gain_db();
    run(&mut lv, &tone(300.0, -50.0), true);
    assert!((lv.gain_db() - before).abs() < 0.5, "{before:.1} -> {:.1} dB", lv.gain_db());
    // The own voice afterwards is still at the target from its first word.
    let out = run(&mut lv, &tone(0.5, -30.0), true);
    assert!((rms_db(&out) - DEFAULT_TARGET_DB).abs() < 0.5, "{:.1} dBFS", rms_db(&out));
  }

  #[test]
  fn a_remembered_level_applies_at_once_and_still_adapts_quickly() {
    let mut lv = VoiceLeveler::new();
    lv.seed(-36.0);
    assert!((lv.gain_db() - 12.0).abs() < 0.1, "first word already gets the gain");
    // Today the voice is 6 dB louder: after 3 s of speech the gain is most of the way there.
    run(&mut lv, &tone(3.0, -30.0), true);
    assert!(lv.gain_db() < 7.5, "{:.1} dB", lv.gain_db());
    // A seed never overrides what this stream learned.
    lv.seed(-50.0);
    assert!(lv.gain_db() < 7.5);
  }

  #[test]
  fn a_settings_change_keeps_the_gain_and_learns_fast_again() {
    let settled = || {
      let mut lv = VoiceLeveler::new();
      run(&mut lv, &tone(60.0, -30.0), true); // fully settled: the slowest rate
      lv
    };
    let before = settled().gain_db();
    // Mic volume turned up 10 dB.
    let mut lv = settled();
    lv.relearn();
    assert!((lv.gain_db() - before).abs() < 0.01, "level and gain kept");
    lv.seed(-50.0); // the remembered level from an earlier stream must not come back
    let out = run(&mut lv, &tone(2.0, -20.0), true);
    assert!((rms_db(&out[SR..]) - DEFAULT_TARGET_DB).abs() < 1.5, "{:.1} dBFS", rms_db(&out[SR..]));
    // Mic volume turned down 10 dB: the power mean follows slower down, still far faster than
    // without the restart.
    let (mut fast, mut slow) = (settled(), settled());
    fast.relearn();
    run(&mut fast, &tone(3.0, -40.0), true);
    run(&mut slow, &tone(3.0, -40.0), true);
    assert!(fast.gain_db() - before > 6.5, "{:.1} dB", fast.gain_db() - before);
    assert!(slow.gain_db() - before < 1.0, "{:.1} dB", slow.gain_db() - before);
  }

  #[test]
  fn an_input_gain_change_is_compensated_at_once() {
    let mut lv = VoiceLeveler::new();
    run(&mut lv, &tone(10.0, -30.0), true);
    let learned = lv.level_db().unwrap();
    // Windows mic volume down 10 dB: the input drops 10 dB and the leveler is told so.
    lv.set_input_gain_db(-10.0);
    let out = run(&mut lv, &tone(1.0, -40.0), true);
    assert!((rms_db(&out[SR / 2..]) - DEFAULT_TARGET_DB).abs() < 0.3, "{:.1} dBFS", rms_db(&out[SR / 2..]));
    assert!((lv.level_db().unwrap() - learned).abs() < 0.1, "the level without the gain stays");
    assert_eq!(lv.speech_ms(), 11_000);
  }

  #[test]
  fn target_setting_moves_the_output() {
    let mut lv = VoiceLeveler::new();
    lv.set_target_db(-18.0);
    let out = run(&mut lv, &tone(3.0, -36.0), true);
    assert!((rms_db(&out[2 * SR..]) + 18.0).abs() < 0.5);
    lv.set_target_db(-80.0); // clamped; the applied gain then ramps there
    let target = 20.0 * lv.target_gain.log10();
    assert!((target - (MIN_TARGET_DB + 36.0)).abs() < 0.5, "{target:.1} dB");
  }
}
