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
use super::{DuckSettings, DuckStatus, DuckTuning, FRESH_HNS, dbg};
use crate::streams::{ENVELOPE_BLOCK_HNS, ENVELOPE_LEN, StreamBus, StreamTable};
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// A chat block's envelope peak above this (-60 dBFS) is a sound: broadband and before the voice
/// detector on purpose, so any chat sound holds the game's level, speech or not.
const CHAT_SOUND_PEAK: f32 = 0.001;
/// The held game level is a mean over the latest chat-free stretch: a running mean from its start
/// (a short pause sees only the game of that pause), then over about this long (a long stretch
/// averages a fight's bursts).
const FREE_MEAN_S: f32 = 1.0;
/// The loud level learns only while the applied depth is below this: the level undoes the depth on
/// the assumption that the caller applied it, which a failed or late volume change or a sound the
/// service does not lower (another app counted as game) would turn into a too-loud level, stored
/// for the next run. It is learned between sentences, when the game is heard as it is.
const LEARN_BELOW_DEPTH_DB: f32 = 0.5;
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
  /// Seconds since the chat last sounded (the guard), and the current chat-free stretch, seconds.
  since_chat_s: f32,
  free_s: f32,
  /// The game's band level, mean square, with this ducker's own lowering undone: the mean over the
  /// latest chat-free stretch, held while the chat sounds.
  game_ms: f32,
  /// The depth the last call returned: the caller applies it within one tick, so the next mix
  /// already carries it.
  last_depth_db: f32,
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
pub fn chat_sounding(table: &StreamTable, now_hns: u64, window_s: f32) -> bool {
  let blocks = (f64::from(window_s.max(0.0)) * 1e7 / ENVELOPE_BLOCK_HNS as f64).ceil() as u64;
  let last = now_hns / ENVELOPE_BLOCK_HNS;
  let first = last.saturating_sub(blocks.clamp(1, ENVELOPE_LEN as u64) - 1);
  table.slots.iter().any(|slot| {
    StreamBus::from_u32(slot.bus.load(Ordering::Relaxed)) == StreamBus::Chat
      && (first..=last).any(|block| {
        let mut peak = [0.0f32];
        slot.envelope(block, &mut peak);
        peak[0] > CHAT_SOUND_PEAK
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
      free_s: 0.0,
      game_ms: 0.0,
      last_depth_db: 0.0,
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
        self.free_s += dt;
        let weight = if self.free_s < FREE_MEAN_S { dt / self.free_s } else { 1.0 - (-dt / FREE_MEAN_S).exp() };
        self.game_ms += (game_ms - self.game_ms) * weight;
        measured = true;
      }
    }
    if !chat_free {
      self.free_s = 0.0; // the next pause starts a new stretch
    }
    let speech_ms = if chat_speech_ms.is_finite() && chat_speech_ms > 0.0 { chat_speech_ms } else { 0.0 };
    let learn_loud = measured && self.last_depth_db <= LEARN_BELOW_DEPTH_DB;
    self.last_depth_db = self.gain.update(dt, speech_ms, self.game_ms, learn_loud, settings, &self.tuning);
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

  /// The settled loud and quiet talker levels, for the per-game store.
  pub fn learned(&self) -> (Option<f32>, Option<f32>) {
    self.gain.settled_levels(&self.tuning)
  }

  /// A discontinuity in the reference (the reader stalled, the writer restarted): the band-pass
  /// starts over and the next chat-free stretch is a new one; the held game level, the decision and
  /// the depth carry on, so the game's volume does not jump in the middle of speech.
  pub fn resync(&mut self) {
    self.state = [[0.0; 2]; 2];
    self.free_s = 0.0;
  }

  /// Leaving the mix path: what depends on past audio starts over, the depth included; what was
  /// learned stays.
  pub fn reset(&mut self) {
    self.gain.restart();
    self.state = [[0.0; 2]; 2];
    (self.since_chat_s, self.free_s, self.game_ms, self.last_depth_db) = (f32::INFINITY, 0.0, 0.0, 0.0);
  }
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests;
