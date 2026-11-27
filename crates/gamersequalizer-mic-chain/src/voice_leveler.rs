//! Learning voice leveler: brings speech to one level whatever the mic gain or how loud the user
//! talks today. It learns the speech level only from frames the voice detector heard as speech
//! (never from noise, keys or whisper, which the detector doesn't hear), fast at first and then
//! ever slower: a running mean (rate 1/n) until it has `MEMORY_FRAMES` of speech, then a fixed
//! rate, i.e. it remembers roughly the last 20 s of speech. The gain only moves while it learns,
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
/// A level remembered from an earlier stream counts like this many speech frames (3 s), so a loud
/// first sentence only nudges it; a level that no longer fits is replaced by the checks below.
const WARM_START_FRAMES: u32 = 300;
/// Gain range. +24 dB lifts a quiet voice on a mic set 12 dB low (private recording: own voice
/// ~-31 dBFS active level, so -12 dB needs ~+19); room noise between words stays under -80 dBFS
/// with the gate on. The cut tames a hot mic before the limiter: -6 dB left a voice at -10 dBFS
/// ~5 dB over the target (`chain::chain_report`, "+20 dB clipped"); cutting never raises noise.
pub const MAX_BOOST_DB: f32 = 24.0;
pub const MAX_CUT_DB: f32 = -18.0;
/// Speech frames before any gain is applied: one syllable is no level yet.
const MIN_FRAMES: u32 = 30;
/// Running mean up to this many speech frames (20 s), then this fixed rate: ever slower while the
/// user talks, so breathing (louder on a fresh breath, softer towards its end) barely moves the
/// gain. 6 s followed every breath (log 2026-11-15 16:15). The start is guarded separately: a
/// remembered level counts as `WARM_START_FRAMES` and is checked after 1 s (`RECHECK_DB`), and
/// settings arriving before the first word no longer restart learning (log 2026-11-15 15:38).
const MEMORY_FRAMES: u32 = 2000;
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
/// After `seed` or `relearn` the first `RECHECK_FRAMES` of speech (1 s) are also averaged as heard;
/// when that is more than `RECHECK_DB` from the kept level it replaces it (log 2026-11-15 14:47:
/// seed -34 dBFS, voice -51.5; a power mean came down so slowly, and `FOREIGN_DB` then dropped the
/// real voice, that it was still +12 dB instead of +24 after 10 s of speech).
const RECHECK_FRAMES: u32 = 100;
const RECHECK_DB: f32 = 8.0;
/// In that check one frame counts as at most this far from the kept level: a plain power mean of
/// one second jumped 10 dB on a short loud sound taken for speech (cough, laugh, bump; log
/// 2026-11-15 16:08: voice -49 dBFS, check -37, gain +20 -> +11). A level that really is off by
/// more is moved this far, and learning (not yet clamped then) does the rest within a second or two.
const RECHECK_CLAMP_DB: f32 = 12.0;
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
  /// The 1 s check after `seed` / `relearn`: speech frames so far and their mean, linear power.
  fresh_frames: u32,
  fresh_raw: f32,
  /// The level the check compares against: the one kept when it started (learning moves on).
  fresh_ref: f32,
  /// The level came from `seed` or `relearn` (and the check above is due).
  prior: bool,
  /// Speech learned since the last `reset` / new stream.
  heard_here: bool,
  /// `set_hold`: an app's own gain control moves the input; and the gain then held, as its
  /// distance from the target in dB (None until there is a level to hold).
  hold: bool,
  held_over_target_db: Option<f32>,
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
      fresh_frames: 0,
      fresh_raw: 0.0,
      fresh_ref: 0.0,
      prior: false,
      heard_here: false,
      hold: false,
      held_over_target_db: None,
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
    self.prior = false;
    self.heard_here = false;
    self.held_over_target_db = None; // the hold is a setting: kept, it takes the next level
    self.restart_check();
  }

  /// Hold: the gain stays where it is (it learns nothing and no longer compensates the input
  /// gain), so an app's own automatic gain control hears its steps and settles. The target still
  /// moves the held gain by the same dB. Without a level yet it learns one first, then holds it.
  /// No allocation: audio thread.
  pub fn set_hold(&mut self, hold: bool) {
    if hold != self.hold {
      self.hold = hold;
      self.held_over_target_db = None;
      self.update_target_gain();
    }
  }

  /// Something before the leveler changed the speech level (mic volume, denoiser, EQ, ...): learn
  /// fast again. The learned level and the gain stay; the level only counts as `MIN_FRAMES` (0.3 s)
  /// of speech, so the next windows outweigh it, and the 1 s check starts over.
  /// Ignored before any speech in this stream: settings arriving in the first blocks are no change
  /// the remembered level has seen (log 2026-11-15: every stream start cut the seed's weight).
  /// No allocation: audio thread.
  pub fn relearn(&mut self) {
    if !self.heard_here {
      return;
    }
    // Fewer frames: already learning fast (and below MIN_FRAMES there is no level to keep).
    self.frames = self.frames.min(MIN_FRAMES);
    self.prior = self.frames >= MIN_FRAMES;
    self.restart_check();
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

  /// Gain (dB) that would bring the learned speech to `DEFAULT_TARGET_DB`, whatever the user's
  /// target and the gain limits: how far the mic is from call level (the echo canceller's mic
  /// gain). None until there is a level.
  pub fn gain_to_default_db(&self) -> Option<f32> {
    self.level_db().map(|level| DEFAULT_TARGET_DB - level - self.input_gain_db)
  }

  /// Speech heard so far (ms, wraps): how long the user talked, for the automatic input gain.
  pub fn speech_ms(&self) -> u32 {
    self.speech_frames.wrapping_mul(10)
  }

  /// Gain applied to the mic before this chain (Windows mic volume), dB. The learned level stays;
  /// the applied gain follows at once (within the ~100 ms smoothing).
  pub fn set_input_gain_db(&mut self, gain_db: f32) {
    // Windows mic volumes span about -96..+30 dB; anything beyond is garbage from the mapping.
    let gain_db = gain_db.clamp(-100.0, 60.0);
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
  /// right gain; it counts as 3 s of speech, and the first second replaces it if the two disagree
  /// by more than `RECHECK_DB`. Ignored once it learned here.
  pub fn seed(&mut self, level_db: f32) {
    if self.started || !level_db.is_finite() {
      return;
    }
    self.started = true;
    self.level_pow = 10f32.powf(level_db / 10.0);
    self.frames = WARM_START_FRAMES;
    self.prior = true;
    self.restart_check();
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
    if !voice || !(pow > 1e-12 && pow.is_finite()) || self.held_over_target_db.is_some() {
      return;
    }
    let pow = pow / self.input_pow;
    self.heard_here = true;
    if self.prior && self.fresh_frames < RECHECK_FRAMES {
      self.fresh_frames += 1;
      let limit = 10f32.powf(RECHECK_CLAMP_DB / 10.0);
      let heard = pow.clamp(self.fresh_ref / limit, self.fresh_ref * limit);
      self.fresh_raw += (heard - self.fresh_raw) / self.fresh_frames as f32;
      let apart_db = 10.0 * (self.fresh_raw / self.fresh_ref.max(1e-12)).log10();
      if self.fresh_frames == RECHECK_FRAMES && apart_db.abs() > RECHECK_DB {
        self.speech_frames = self.speech_frames.wrapping_add(1);
        self.level_pow = self.fresh_raw;
        self.frames = RECHECK_FRAMES;
        self.update_target_gain();
        return;
      }
    }
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

  fn restart_check(&mut self) {
    self.fresh_frames = 0;
    self.fresh_raw = 0.0;
    self.fresh_ref = self.level_pow;
  }

  fn update_target_gain(&mut self) {
    let gain_db = match (self.held_over_target_db, self.level_db()) {
      (Some(over), _) => self.target_db + over,
      (None, Some(level)) => self.target_db - level - self.input_gain_db,
      (None, None) => return,
    };
    let gain_db = gain_db.clamp(MAX_CUT_DB, MAX_BOOST_DB);
    self.target_gain = 10f32.powf(gain_db / 20.0);
    if self.hold && self.held_over_target_db.is_none() {
      self.held_over_target_db = Some(gain_db - self.target_db);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const SR: usize = 48_000;

  /// Speech stand-in: a 200 Hz tone at `rms_db`, flagged as voice.
  fn tone(secs: f32, rms_db: f32) -> Vec<f32> {
    let amp = 10f32.powf(rms_db / 20.0) * std::f32::consts::SQRT_2;
    (0..(secs * SR as f32) as usize)
      .map(|i| amp * (i as f32 * 200.0 * std::f32::consts::TAU / SR as f32).sin())
      .collect()
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
  fn breathing_barely_moves_the_gain_once_settled() {
    // Log 2026-11-15 16:15: a fresh breath is louder, the end of it softer; 6 s of memory followed
    // every breath (0.7 dB swing here, 20 s: 0.3).
    let mut lv = VoiceLeveler::new();
    run(&mut lv, &tone(30.0, -40.0), true);
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for _ in 0..8 {
      for (secs, db) in [(1.0, -37.0), (1.5, -40.0), (1.5, -43.0)] {
        run(&mut lv, &tone(secs, db), true);
        lo = lo.min(lv.gain_db());
        hi = hi.max(lv.gain_db());
      }
    }
    assert!(hi - lo < 0.5, "gain swung {lo:.1} .. {hi:.1} dB");
  }

  #[test]
  fn a_softer_day_is_followed_slowly_but_surely() {
    let mut lv = VoiceLeveler::new();
    run(&mut lv, &tone(60.0, -30.0), true);
    let normal = lv.gain_db();
    // 10 dB quieter after a minute of talking: a few softer sentences (or the end of a breath) barely
    // move the gain; a voice that stays softer is followed within about a minute of speech.
    run(&mut lv, &tone(6.0, -40.0), true);
    let early = lv.gain_db() - normal;
    assert!(early > 0.5 && early < 3.0, "{normal:.1} -> {:.1} dB", lv.gain_db());
    run(&mut lv, &tone(84.0, -40.0), true);
    let quieter = lv.gain_db();
    assert!((quieter - normal - 10.0).abs() < 1.0, "{normal:.1} -> {quieter:.1} dB");
  }

  #[test]
  fn a_loud_first_sentence_doesnt_hold_the_level_up() {
    // Log 2026-11-15 15:38: a first second at -44.5 dBFS, then the normal voice at ~-50.
    let mut lv = VoiceLeveler::new();
    run(&mut lv, &tone(1.0, -40.5), true); // 4 dB above the log, so +24 dB does not cap it
    let out = run(&mut lv, &tone(11.0, -46.0), true);
    assert!((rms_db(&out[9 * SR..]) - DEFAULT_TARGET_DB).abs() < 1.0, "{:.1} dBFS", rms_db(&out[9 * SR..]));
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
    // Today the voice is 6 dB louder: the first second only nudges it (the seed counts as 3 s; a
    // loud first sentence must not drop the gain, log 2026-11-15 15:38), then it follows smoothly.
    run(&mut lv, &tone(1.0, -30.0), true);
    assert!(lv.gain_db() > 9.0 && lv.gain_db() < 11.5, "{:.1} dB", lv.gain_db());
    run(&mut lv, &tone(11.0, -30.0), true);
    assert!((lv.gain_db() - 6.0).abs() < 1.0, "{:.1} dB", lv.gain_db());
    // A seed never overrides what this stream learned.
    let learned = lv.gain_db();
    lv.seed(-50.0);
    assert_eq!(lv.gain_db(), learned);
  }

  #[test]
  fn a_loud_first_second_barely_dips_a_remembered_gain() {
    // Log 2026-11-15 15:38: remembered -51.5 dBFS (+24 dB), first second at -44.5, then ~-50.
    let mut lv = VoiceLeveler::new();
    lv.seed(-51.5);
    lv.relearn(); // settings arriving at the stream start
    run(&mut lv, &tone(1.0, -44.5), true);
    let mut lowest = lv.gain_db();
    for _ in 0..8 {
      run(&mut lv, &tone(0.5, -50.0), true);
      lowest = lowest.min(lv.gain_db());
    }
    assert!(lowest > 20.0, "dipped to {lowest:.1} dB (was +17 dB)");
    assert!(lv.gain_db() > 23.0, "{:.1} dB after 5 s", lv.gain_db());
  }

  #[test]
  fn a_short_loud_sound_in_the_first_second_keeps_the_remembered_level() {
    // Log 2026-11-15 16:08: voice and remembered level -49 dBFS, a short loud sound taken for speech.
    let mut lv = VoiceLeveler::new();
    lv.seed(-45.0);
    let mut x = tone(0.5, -45.0);
    x.extend(tone(0.03, -10.0));
    x.extend(tone(0.6, -45.0));
    run(&mut lv, &x, true);
    assert!(lv.gain_db() > 19.0, "{:.1} dB (was +11)", lv.gain_db());
  }

  #[test]
  fn settings_arriving_before_the_first_word_keep_the_remembered_level() {
    // Log 2026-11-15: every stream start saw "voice EQ changed" before any speech.
    let mut lv = VoiceLeveler::new();
    lv.seed(-36.0);
    lv.relearn();
    run(&mut lv, &tone(1.1, -30.0), true);
    assert!(lv.gain_db() > 9.0, "{:.1} dB", lv.gain_db());
  }

  #[test]
  fn a_far_off_remembered_level_is_replaced_after_a_second_of_speech() {
    // Log 2026-11-15: remembered -34 dBFS, today's voice -51.5 (mic further away, other model).
    let mut lv = VoiceLeveler::new();
    lv.seed(-34.0);
    run(&mut lv, &tone(2.5, -51.5), true); // the check moves it 12 dB, learning the rest
    assert!(lv.gain_db() > 23.5, "{:.1} dB", lv.gain_db());
    // The same after "learning fast again" with a level that no longer fits.
    let mut lv = VoiceLeveler::new();
    run(&mut lv, &tone(60.0, -34.0), true);
    lv.relearn();
    let out = run(&mut lv, &tone(2.0, -46.0), true);
    assert!((rms_db(&out[3 * SR / 2..]) - DEFAULT_TARGET_DB).abs() < 0.5, "{:.1} dBFS", rms_db(&out[3 * SR / 2..]));
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
    // The 1 s check replaces the level; the gain follows within its ~100 ms smoothing.
    assert!((rms_db(&out[13 * SR / 10..]) - DEFAULT_TARGET_DB).abs() < 1.0, "{:.1} dBFS", rms_db(&out[13 * SR / 10..]));
    // Mic volume turned down 10 dB: replaced after 1 s of speech, far faster than without the
    // restart.
    let (mut fast, mut slow) = (settled(), settled());
    fast.relearn();
    run(&mut fast, &tone(1.5, -40.0), true);
    run(&mut slow, &tone(1.5, -40.0), true);
    assert!((fast.gain_db() - before - 10.0).abs() < 1.0, "{:.1} dB", fast.gain_db() - before);
    assert!(slow.gain_db() - before < 2.0, "{:.1} dB", slow.gain_db() - before);
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
    // Regression: clamped at -60 dB, a mic volume near the bottom (Windows reaches ~-96 dB) was
    // taken as up to 36 dB louder than it is.
    lv.set_input_gain_db(-90.0);
    assert_eq!(lv.input_gain_db, -90.0);
    lv.set_input_gain_db(-500.0);
    assert_eq!(lv.input_gain_db, -100.0);
  }

  #[test]
  fn a_hold_keeps_the_gain_while_the_input_moves() {
    let mut lv = VoiceLeveler::new();
    run(&mut lv, &tone(10.0, -30.0), true);
    let learned = (lv.level_db().unwrap(), lv.gain_db());
    lv.set_hold(true);
    // An app raises the Windows volume 10 dB: the gain stays, the app hears its step.
    lv.set_input_gain_db(10.0);
    let out = run(&mut lv, &tone(5.0, -20.0), true);
    assert!((lv.gain_db() - learned.1).abs() < 0.05, "{:.1} -> {:.1} dB", learned.1, lv.gain_db());
    assert!((rms_db(&out[SR..]) - (DEFAULT_TARGET_DB + 10.0)).abs() < 0.3, "{:.1} dBFS", rms_db(&out[SR..]));
    assert_eq!(lv.level_db().unwrap(), learned.0, "nothing learned while held");
    // The user's target still moves the held gain by the same dB.
    lv.set_target_db(DEFAULT_TARGET_DB - 6.0);
    run(&mut lv, &tone(1.0, -20.0), true);
    assert!((lv.gain_db() - (learned.1 - 6.0)).abs() < 0.05, "{:.1} dB", lv.gain_db());
    // Released: the input gain is compensated again, at once.
    lv.set_hold(false);
    let out = run(&mut lv, &tone(1.0, -20.0), true);
    assert!((rms_db(&out[SR / 2..]) - (DEFAULT_TARGET_DB - 6.0)).abs() < 0.3, "{:.1} dBFS", rms_db(&out[SR / 2..]));

    // Held before any level: it learns one first, then holds that gain.
    let mut lv = VoiceLeveler::new();
    lv.set_hold(true);
    run(&mut lv, &tone(3.0, -36.0), true);
    let held = lv.gain_db();
    assert!((held - 12.0).abs() < 1.0, "{held:.1} dB");
    lv.set_input_gain_db(-6.0);
    run(&mut lv, &tone(1.0, -42.0), true);
    assert!((lv.gain_db() - held).abs() < 0.05, "{held:.1} -> {:.1} dB", lv.gain_db());
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
