//! The service's ducker, for a game whose own stream the stream APO cannot see: a game that renders
//! spatial audio objects (Dolby Atmos) bypasses the per-stream effects (DEVLOG Milestone 19). The
//! game's level comes from the output mix (the speaker reference the output APO writes), and the
//! service applies the depth to the game's session volume, full band. The decision is the stream
//! ducker's (`DuckGain`): per spurt, the learned levels, the lock, the slews.
//!
//! The game is measured only while the chat makes no sound at all (`chat_sounding` plus a guard):
//! then the mix is the game, and anything else in it that is not chat. While the chat sounds, the
//! game's level from before is held; nothing is learned and nothing moves it. A game's loudness
//! barely changes within a run and nobody talks through every fight, so the pauses are enough, and
//! the chat can never be mistaken for the game, whatever its level in the mix. The costs: while
//! someone talks, the game is the one from before they started; a fight that starts or ends in the
//! middle of speech is noticed at the next pause longer than the guard; someone who talks without
//! any pause freezes it.

use super::band::BandPass;
use super::gain::DuckGain;
use super::{DuckSettings, DuckStatus, DuckTuning, FRESH_HNS, db, dbg};
use crate::streams::{ENVELOPE_BLOCK_HNS, ENVELOPE_LEN, StreamBus, StreamTable};
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// The game is measured in segments: a chat-free stretch, cut every this long (a long stretch
/// averages a fight's bursts). The held level rises only to what two segments in a row read, the
/// lower of their means: one shot in a calm game fills one segment, a fight fills them all.
const SEGMENT_S: f32 = 1.0;
/// The most game time one measurement stands for in the loud level: a single short pause in a
/// loud moment must not set it at once.
const MAX_LOUD_STEP_S: f32 = 2.0;
/// The loud level learns freely only while the applied depth is below this. While lowered it
/// learns too (the depth is undone), but never above the game level read when the depth was last
/// below this: the undo also lifts whatever the service does not lower (another app, a volume change
/// that failed), which would otherwise raise the depth, then itself, up to the bound, and be stored
/// for the next run. Cost: a fight that starts while a calmer game is already lowered is learned
/// only once the reduction has come back.
const UNLOWERED_DB: f32 = 0.5;
/// Longest step `process` takes; a longer stall of the caller counts as this.
const MAX_STEP_S: f32 = 0.2;

// ---------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------

/// One per ducked game (the service's tick drives it).
#[derive(Debug, Clone)]
pub struct MixDucker {
  sample_rate: f32,
  band: BandPass,
  /// Band-pass state of the stereo mix.
  state: [[f64; 2]; 2],
  gain: DuckGain,
  /// Seconds since the chat last sounded (the guard).
  since_chat_s: f32,
  /// The current segment: its game level summed over time (mean square x seconds) and its length;
  /// the mean of the segment before it.
  seg_sum: f32,
  seg_s: f32,
  prev_seg_ms: Option<f32>,
  /// Seconds since the game was last measured.
  unmeasured_s: f32,
  /// The game's band level, mean square, with this ducker's own lowering undone: what the latest
  /// segments read, held while the chat sounds.
  game_ms: f32,
  /// The depth the last call returned: the caller applies it within one tick, so the next mix
  /// already carries it.
  last_depth_db: f32,
  /// The held game level when the depth was last below `UNLOWERED_DB`: the loud level's cap while
  /// lowered.
  unlowered_ms: f32,
  /// The stored levels `seed` adopted.
  loud_seeded: bool,
  quiet_seeded: bool,
  /// The last call's tuning, with the depth bound of the full-band path.
  tuning: DuckTuning,
}

// ---------------------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------------------

/// True when any fresh Chat slot's level envelope (`env_peak`, one peak per 10 ms block, always
/// recorded by the stream APO) exceeded the sound threshold within the last `window_s` seconds.
/// The envelope holds only the blocks a stream recorded, so a peak inside the window is fresh by
/// itself: no separate freshness check, which (50 ms) would cut a window of a tick and a margin.
/// `peak_db`: the sound threshold (`mix_chat_sound_db`); broadband and before the voice detector on
/// purpose, so any chat sound above it holds the game's level, speech or not.
pub fn chat_sounding(table: &StreamTable, now_hns: u64, window_s: f32, peak_db: f32) -> bool {
  let threshold = 10f32.powf(peak_db / 20.0);
  let blocks = (f64::from(window_s.max(0.0)) * 1e7 / ENVELOPE_BLOCK_HNS as f64).ceil() as u64;
  let last = now_hns / ENVELOPE_BLOCK_HNS;
  let first = last.saturating_sub(blocks.clamp(1, ENVELOPE_LEN as u64) - 1);
  table.slots.iter().any(|slot| {
    StreamBus::from_u32(slot.bus.load(Ordering::Relaxed)) == StreamBus::Chat
      && (first..=last).any(|block| {
        let mut peak = [0.0f32];
        slot.envelope(block, &mut peak);
        peak[0] > threshold
      })
  })
}

/// Chat levels summed over the fresh Chat slots of the table: (speech_ms, band_ms).
/// speech_ms: the detector-gated level (`StreamSlot::speech_ms`); band_ms: the raw band level
/// (`duck_dbg[dbg::BAND_DB]`, dB x 10 -> mean square; `NA` counts as silence).
pub fn chat_levels(table: &StreamTable, now_hns: u64) -> (f32, f32) {
  let (mut speech, mut band) = (0.0f32, 0.0f32);
  for slot in &table.slots {
    let fresh = now_hns.saturating_sub(slot.last_audio_hns.load(Ordering::Relaxed)) <= FRESH_HNS;
    if !fresh || StreamBus::from_u32(slot.bus.load(Ordering::Relaxed)) != StreamBus::Chat {
      continue;
    }
    // Written by another process: never let a bad value through.
    let ms = f32::from_bits(slot.speech_ms.load(Ordering::Relaxed));
    speech += if ms.is_finite() && ms > 0.0 { ms } else { 0.0 };
    let x10 = slot.duck_dbg.get(dbg::BAND_DB).map_or(dbg::NA, |cell| cell.load(Ordering::Relaxed));
    let ms = if x10 == dbg::NA { 0.0 } else { 10f32.powf(x10 as f32 / 100.0) };
    band += if ms.is_finite() { ms } else { 0.0 };
  }
  (speech, band)
}

impl MixDucker {
  pub fn new(sample_rate: f32) -> Self {
    let sample_rate = if sample_rate.is_finite() && sample_rate >= 8000.0 { sample_rate } else { 48_000.0 };
    MixDucker {
      sample_rate,
      band: BandPass::new(f64::from(sample_rate), &DuckTuning::DEFAULT),
      state: [[0.0; 2]; 2],
      gain: DuckGain::default(),
      since_chat_s: f32::INFINITY,
      seg_sum: 0.0,
      seg_s: 0.0,
      prev_seg_ms: None,
      unmeasured_s: 0.0,
      game_ms: 0.0,
      last_depth_db: 0.0,
      unlowered_ms: 0.0,
      loud_seeded: false,
      quiet_seeded: false,
      tuning: DuckTuning::DEFAULT,
    }
  }

  /// `mix`: interleaved stereo samples of the output mix that arrived since the last call;
  /// `dt`: seconds since the last call; `chat_speech_ms`: the chat's detector-gated speech level
  /// (`chat_levels`); `chat_sounding`: the chat made any sound during this tick (`chat_sounding`).
  /// Returns the full-band depth (dB, >= 0) to apply to the game's volume now.
  pub fn process(
    &mut self,
    mix: &[f32],
    dt: f32,
    chat_speech_ms: f32,
    chat_sounding: bool,
    settings: &DuckSettings,
    tuning: &DuckTuning,
  ) -> f32 {
    if (tuning.band_center_hz, tuning.band_q) != (self.tuning.band_center_hz, self.tuning.band_q) {
      self.band = BandPass::new(f64::from(self.sample_rate), tuning);
    }
    // This depth is full band: bounded by its own maximum.
    self.tuning = DuckTuning {
      max_depth_db: tuning.max_depth_db.min(tuning.mix_max_depth_db),
      hold_s: tuning.mix_hold_s,
      max_margin_db: tuning.mix_max_margin_db,
      release_db_per_s: tuning.mix_release_db_per_s,
      ..*tuning
    };
    if !dt.is_finite() || dt <= 0.0 {
      return self.last_depth_db;
    }
    let dt = dt.min(MAX_STEP_S);
    // The guard covers the chat's way through the spatial renderer and the tail of a word.
    self.since_chat_s = if chat_sounding { 0.0 } else { self.since_chat_s + dt };
    let chat_free = self.since_chat_s >= tuning.mix_chat_guard_s;
    let mut measured = false;
    if !mix.is_empty() {
      // The band-pass runs on every tick, so its state is continuous when a pause starts.
      let mix_ms = self.band.measure(&mut self.state, mix, 2);
      if chat_free {
        // The mix carries the game lowered by the depth returned last time (the caller applies it
        // within one tick): undo that.
        let game_ms = mix_ms * 10f32.powf(self.last_depth_db / 10.0);
        // The segment before ages like the held level: one from a fight long ago confirms nothing.
        if let Some(prev) = self.prev_seg_ms.as_mut() {
          *prev *= (-self.unmeasured_s / tuning.mix_game_fall_s).exp();
        }
        if self.seg_s >= SEGMENT_S {
          self.close_segment();
        }
        self.seg_sum += game_ms * dt;
        self.seg_s += dt;
        if game_ms < self.game_ms {
          // The fall counts the time since the last measurement too: the pauses that measure the
          // game are short and far apart when someone talks a lot.
          let weight = 1.0 - (-(dt + self.unmeasured_s) / tuning.mix_game_fall_s).exp();
          self.game_ms += (game_ms - self.game_ms) * weight;
        }
        let seg_ms = self.seg_sum / self.seg_s;
        self.game_ms = self.game_ms.max(self.prev_seg_ms.map_or(seg_ms, |prev| prev.min(seg_ms)));
        measured = true;
      }
    }
    if !chat_free && self.seg_s > 0.0 {
      self.close_segment(); // the next pause starts a new segment
    }
    // A measurement stands for the time since the previous one in the loud level too, or a talker
    // with few pauses would keep it from ever learning a fight.
    let loud_dt = if measured { (dt + self.unmeasured_s).min(MAX_LOUD_STEP_S) } else { 0.0 };
    let unlowered = self.last_depth_db <= UNLOWERED_DB;
    if unlowered {
      self.unlowered_ms = self.game_ms;
    }
    let loud = (loud_dt, if unlowered { f32::INFINITY } else { db(self.unlowered_ms) });
    self.unmeasured_s = if measured { 0.0 } else { self.unmeasured_s + dt };
    let speech_ms = if chat_speech_ms.is_finite() && chat_speech_ms > 0.0 { chat_speech_ms } else { 0.0 };
    self.last_depth_db = self.gain.update(dt, speech_ms, self.game_ms, loud, settings, &self.tuning);
    self.last_depth_db
  }

  /// What the log lines are built from.
  pub fn status(&self) -> DuckStatus {
    DuckStatus { loud_seeded: self.loud_seeded, quiet_seeded: self.quiet_seeded, ..self.gain.status(&self.tuning) }
  }

  /// Levels an earlier run of this game stored, given once when the game is paired; each is
  /// adopted unless this ducker's own estimate has already settled.
  pub fn seed(&mut self, loud_db: Option<f32>, quiet_db: Option<f32>) {
    if let Some(db) = loud_db.filter(|v| v.is_finite()) {
      self.loud_seeded = self.gain.seed_loud(db, &self.tuning);
    }
    if let Some(db) = quiet_db.filter(|v| v.is_finite()) {
      self.quiet_seeded = self.gain.seed_quiet(db, &self.tuning);
    }
  }

  /// The game's or the chat's session volume moved by `game_db` / `chat_db`: everything measured
  /// so far is at the old volume, so it moves with it.
  pub fn shift(&mut self, game_db: f32, chat_db: f32) {
    if !(game_db.is_finite() && chat_db.is_finite()) {
      return;
    }
    let factor = 10f32.powf(game_db / 10.0);
    self.game_ms *= factor;
    self.seg_sum *= factor;
    self.prev_seg_ms = self.prev_seg_ms.map(|ms| ms * factor);
    self.unlowered_ms *= factor;
    self.gain.shift(game_db, chat_db);
  }

  /// The settled loud and quiet talker levels, for the per-game store.
  pub fn learned(&self) -> (Option<f32>, Option<f32>) {
    self.gain.settled_levels(&self.tuning)
  }

  /// A discontinuity in the reference (the reader stalled, the writer restarted): the band-pass
  /// starts over and the next chat-free stretch is a new one; the held game level, the decision and
  /// the depth carry on, so the game's volume does not jump in the middle of speech.
  pub fn resync(&mut self) {
    self.state = [[0.0; 2]; 2];
    (self.seg_sum, self.seg_s) = (0.0, 0.0);
  }

  /// Leaving the mix path: what depends on past audio starts over, the depth included; what was
  /// learned stays.
  pub fn reset(&mut self) {
    self.gain.restart();
    self.state = [[0.0; 2]; 2];
    (self.since_chat_s, self.seg_sum, self.seg_s, self.prev_seg_ms) = (f32::INFINITY, 0.0, 0.0, None);
    (self.unmeasured_s, self.game_ms, self.last_depth_db, self.unlowered_ms) = (0.0, 0.0, 0.0, 0.0);
  }

  fn close_segment(&mut self) {
    self.prev_seg_ms = Some(self.seg_sum / self.seg_s.max(f32::MIN_POSITIVE));
    (self.seg_sum, self.seg_s) = (0.0, 0.0);
  }
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests;
