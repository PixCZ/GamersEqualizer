//! The service's ducker, for a game whose own stream the stream APO cannot see: a game that renders
//! spatial audio objects (Dolby Atmos) bypasses the per-stream effects (DEVLOG Milestone 19). The
//! game's level comes from the output mix (the speaker reference the output APO writes), and the
//! service applies the depth to the game's session volume, full band. The decision is the stream
//! ducker's (`DuckGain`): the lowering level while someone talks, learned from the fights, and the
//! slews.
//!
//! While the chat makes no sound at all (`chat_sounding` plus a guard) the mix is the game, and
//! anything else in it that is not chat: the game is measured, up and down. While the chat sounds,
//! the mix less the chat's own level (its stream level at the output volume, peak-held, with a
//! margin) is a lower bound of the game: it can raise the held level, so a fight that starts in the
//! middle of speech is heard within a few seconds, but it never lowers it. It relies on the chat
//! sitting in the mix at its stream's level (live log 2026-11-19: its peaks in the mix were at its
//! stream's peaks, or just under). The fall waits for a pause; someone who talks without any pause
//! holds the level.

use super::band::BandPass;
use super::gain::DuckGain;
use super::{DuckSettings, DuckStatus, DuckTuning, FRESH_HNS, dbg};
use crate::streams::{ENVELOPE_BLOCK_HNS, ENVELOPE_LEN, StreamBus, StreamTable};
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// The game is measured in segments: a chat-free stretch, cut every this long (a long stretch
/// averages a fight's bursts). The held level rises only to what two segments in a row read, the
/// lower of their means: one shot in a calm game fills one segment, a fight fills them all.
const SEGMENT_S: f32 = 1.0;
/// Longest step `process` takes; a longer stall of the caller counts as this.
const MAX_STEP_S: f32 = 0.2;
/// The chat's level held at its peaks falls with this time constant: covers the gaps between
/// words and the tail of the chat in the mix after it stopped sounding.
const CHAT_HOLD_S: f32 = 0.5;
/// How many times the held chat level is taken out of the mix while it sounds (6 dB): the level the
/// stream publishes is one 10 ms block per tick, and speech varies by tens of dB between blocks, so
/// its peaks are missed now and then (`duck_sim` on a podcast: 2x let the chat through as game).
/// shortcut: fixed; a fight less than 6 dB above the chat's peaks is noticed only in a pause.
const CHAT_MARGIN: f32 = 4.0;

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
  /// The chat's band level at the output, mean square, held at its peaks (`CHAT_HOLD_S`).
  chat_held_ms: f32,
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
      chat_held_ms: 0.0,
      seg_sum: 0.0,
      seg_s: 0.0,
      prev_seg_ms: None,
      unmeasured_s: 0.0,
      game_ms: 0.0,
      last_depth_db: 0.0,
      tuning: DuckTuning::DEFAULT,
    }
  }

  /// `mix`: interleaved stereo samples of the output mix that arrived since the last call;
  /// `dt`: seconds since the last call; `chat`: the chat's detector-gated speech level and its raw
  /// band level (`chat_levels`); `chat_sounding`: the chat made any sound during this tick
  /// (`chat_sounding`).
  /// Returns the full-band depth (dB, >= 0) to apply to the game's volume now.
  pub fn process(
    &mut self,
    mix: &[f32],
    dt: f32,
    chat: (f32, f32),
    chat_sounding: bool,
    settings: &DuckSettings,
    tuning: &DuckTuning,
  ) -> f32 {
    if (tuning.band_center_hz, tuning.band_q) != (self.tuning.band_center_hz, self.tuning.band_q) {
      self.band = BandPass::new(f64::from(self.sample_rate), tuning);
    }
    self.tuning = *tuning;
    if !dt.is_finite() || dt <= 0.0 {
      return self.last_depth_db;
    }
    let dt = dt.min(MAX_STEP_S);
    let (chat_speech_ms, chat_band_ms) = chat;
    let at_output = if chat_band_ms.is_finite() && chat_band_ms > 0.0 {
      chat_band_ms * settings.chat_gain * settings.chat_gain
    } else {
      0.0
    };
    self.chat_held_ms = at_output.max(self.chat_held_ms * (-dt / CHAT_HOLD_S).exp());
    // The guard covers the chat's way through the spatial renderer and the tail of a word.
    self.since_chat_s = if chat_sounding { 0.0 } else { self.since_chat_s + dt };
    let chat_free = self.since_chat_s >= tuning.mix_chat_guard_s;
    let mut measured = false;
    if !mix.is_empty() {
      // The band-pass runs on every tick, so its state is continuous when a pause starts.
      let mix_ms = self.band.measure(&mut self.state, mix, 2);
      // Chat free: the mix is the game. Chat sounding: what is left of the mix beyond the chat's
      // held level is at least the game, and only ever raises the held level.
      let heard_ms = if chat_free { mix_ms } else { (mix_ms - CHAT_MARGIN * self.chat_held_ms).max(0.0) };
      // The mix carries the game lowered by the depth returned last time (the caller applies it
      // within one tick): undo that.
      let game_ms = heard_ms * 10f32.powf(self.last_depth_db / 10.0);
      // The segment before ages like the held level: one from a fight long ago confirms nothing.
      if let Some(prev) = self.prev_seg_ms.as_mut().filter(|_| chat_free) {
        *prev *= (-self.unmeasured_s / tuning.mix_game_fall_s).exp();
      }
      if self.seg_s >= SEGMENT_S {
        self.close_segment();
      }
      self.seg_sum += game_ms * dt;
      self.seg_s += dt;
      if chat_free && game_ms < self.game_ms {
        // The fall counts the time since the last measurement too: the pauses that measure the
        // game are short and far apart when someone talks a lot.
        let weight = 1.0 - (-(dt + self.unmeasured_s) / tuning.mix_game_fall_s).exp();
        self.game_ms += (game_ms - self.game_ms) * weight;
      }
      let seg_ms = self.seg_sum / self.seg_s;
      // A fight is two segments in a row; the first one ever stands alone only in a pause (while
      // the chat sounds, its onset can read as game before the held level has caught up).
      let confirmed = self.prev_seg_ms.map_or(if chat_free { seg_ms } else { 0.0 }, |prev| prev.min(seg_ms));
      self.game_ms = self.game_ms.max(confirmed);
      measured = chat_free;
    }
    // The game is measured while lowered as well (the depth undone), or a fight that grows louder
    // under a reduction would never raise the level (live log 2026-11-19: the game rose from -53 to
    // -42 dB under 10 dB). shortcut: the undo also lifts what the service does not lower (another
    // app in the mix, a volume change that failed), which can raise the level up to `max_depth_db`
    // while that sound plays; a deeper duck is the lesser evil. Tell the game from the rest of the
    // mix if it shows in a live log.
    self.unmeasured_s = if measured { 0.0 } else { self.unmeasured_s + dt };
    let speech_ms = if chat_speech_ms.is_finite() && chat_speech_ms > 0.0 { chat_speech_ms } else { 0.0 };
    self.last_depth_db = self.gain.update(dt, speech_ms, self.game_ms, settings, &self.tuning);
    self.last_depth_db
  }

  /// What the log lines are built from.
  pub fn status(&self) -> DuckStatus {
    self.gain.status()
  }

  /// The game's or the chat's session volume moved by `game_db` / `chat_db`: everything measured
  /// so far is at the old volume, so it moves with it, and the lowering level starts over.
  pub fn shift(&mut self, game_db: f32, chat_db: f32) {
    if !(game_db.is_finite() && chat_db.is_finite()) {
      return;
    }
    let factor = 10f32.powf(game_db / 10.0);
    self.game_ms *= factor;
    self.seg_sum *= factor;
    self.prev_seg_ms = self.prev_seg_ms.map(|ms| ms * factor);
    self.gain.shift(game_db, chat_db);
  }

  /// A discontinuity in the reference (the reader stalled, the writer restarted): the band-pass
  /// starts over and the next chat-free stretch is a new one; the held game level, the decision and
  /// the depth carry on, so the game's volume does not jump in the middle of speech.
  pub fn resync(&mut self) {
    self.state = [[0.0; 2]; 2];
    (self.seg_sum, self.seg_s) = (0.0, 0.0);
  }

  /// Leaving the mix path: everything starts over, the depth and the lowering level included.
  pub fn reset(&mut self) {
    self.gain.restart();
    self.state = [[0.0; 2]; 2];
    (self.since_chat_s, self.chat_held_ms, self.seg_sum) = (f32::INFINITY, 0.0, 0.0);
    (self.seg_s, self.prev_seg_ms) = (0.0, None);
    (self.unmeasured_s, self.game_ms, self.last_depth_db) = (0.0, 0.0, 0.0);
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
