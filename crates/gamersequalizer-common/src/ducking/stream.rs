//! The ducker of one app stream, run by its stream APO instance on the audio thread: measures the
//! stream's speech band, publishes it in the stream's slot, reads the other streams' levels from
//! the table, runs the gain computer and lowers the whole stream by the depth, full band like mix
//! mode's session volume (one feel for both). No allocation, no locks.

use super::band::BandPass;
use super::gain::DuckGain;
use super::{DuckRole, DuckSettings, DuckStatus, DuckTuning, FRESH_HNS, GAP_HNS, db, dbg};
use crate::shm::ShmHeader;
use crate::streams::{StreamBus, StreamSlot, StreamTable};
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// Channels with filter state (7.1); a stream with more passes through untouched.
const MAX_CHANNELS: usize = 8;
// ---------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------

/// One per stream instance.
#[derive(Debug, Clone)]
pub struct StreamDucker {
  sample_rate: f32,
  band: BandPass,
  /// Band-pass state (transposed direct form II) per channel.
  state: [[f64; 2]; MAX_CHANNELS],
  gain: DuckGain,
  /// Linear gain of the stream at the end of the last block (1 = untouched).
  last_gain: f32,
  last_hns: Option<u64>,
  /// Not processing, and the slot's level, reduction and telemetry already read "none".
  idle: bool,
  /// The slot's `duck_volume_epoch` last seen: a change starts the lowering level over.
  volume_epoch: i32,
  /// This stream's copy of the tuning and the `duck_tune_version` it was read at.
  tuning: DuckTuning,
  tune_version: Option<u32>,
}

// ---------------------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------------------

impl StreamDucker {
  pub fn new(sample_rate: f32) -> Self {
    // A bogus rate from the engine must not give NaN coefficients or a zero block time.
    let sample_rate = if sample_rate.is_finite() && sample_rate >= 8000.0 { sample_rate } else { 48_000.0 };
    StreamDucker {
      sample_rate,
      band: BandPass::new(f64::from(sample_rate), &DuckTuning::DEFAULT),
      state: [[0.0; 2]; MAX_CHANNELS],
      gain: DuckGain::default(),
      last_gain: 1.0,
      last_hns: None,
      idle: true,
      volume_epoch: 0,
      tuning: DuckTuning::DEFAULT,
      tune_version: None,
    }
  }

  /// Takes a new tuning when `duck_tune_version` moved (one atomic load per block otherwise) and
  /// rebuilds the measuring band-pass from it. Its filter state carries on and the depth still moves
  /// only at the slew rates.
  pub fn refresh_tuning(&mut self, h: &ShmHeader) {
    let version = h.duck_tune_version.load(Ordering::Acquire);
    if self.tune_version != Some(version) {
      self.tuning = DuckTuning::read_from(h);
      self.band = BandPass::new(f64::from(self.sample_rate), &self.tuning);
      self.tune_version = Some(version);
    }
  }

  pub fn tuning(&self) -> &DuckTuning {
    &self.tuning
  }

  /// One block of the stream at `own_index`, interleaved, in place. The buffer is not touched
  /// while no reduction is applied. `is_speech` (Voice role only): the chat stream's voice
  /// detector hears speech; without it the stream publishes no speech level, so coughs, breathing
  /// and room noise never lower the game.
  #[expect(
    clippy::too_many_arguments,
    reason = "the block plus where this stream sits and what it reads, all per call"
  )]
  pub fn process(
    &mut self,
    buf: &mut [f32],
    channels: usize,
    role: DuckRole,
    is_speech: bool,
    table: &StreamTable,
    own_index: usize,
    now_hns: u64,
    settings: &DuckSettings,
  ) {
    let Some(own) = table.slots.get(own_index) else { return };
    let frames = buf.len().checked_div(channels).unwrap_or(0);
    if channels > MAX_CHANNELS || frames == 0 {
      return;
    }
    if self.last_hns.is_some_and(|t| now_hns.saturating_sub(t) > GAP_HNS) {
      self.restart();
    }
    self.last_hns = Some(now_hns);
    let enabled = settings.enabled && role != DuckRole::Other;
    // Off, or the channel was lost: a reduction still applied releases first, never jumps.
    if !enabled && self.gain.depth_db() <= 0.0 {
      self.go_idle(own);
      return;
    }
    self.idle = false;
    let level = self.measure(buf, channels);
    let published = if role == DuckRole::Voice && !is_speech { 0.0 } else { level };
    own.speech_ms.store(published.to_bits(), Ordering::Relaxed);
    dbg::store(own, dbg::BAND_DB, Some(db(level)));
    let ducking = enabled && role == DuckRole::Ducked;
    if !ducking && self.gain.depth_db() <= 0.0 {
      return;
    }
    let (chat_ms, game_ms) = if ducking { levels(table, own_index, level, now_hns) } else { (0.0, 0.0) };
    let epoch = own.duck_volume_epoch.load(Ordering::Relaxed);
    if epoch != self.volume_epoch {
      // The stream's levels already carry the new session volume: only the level starts over.
      self.gain.shift(0.0, 0.0);
      self.volume_epoch = epoch;
    }
    let dt = frames as f32 / self.sample_rate;
    let settings = DuckSettings { enabled: ducking, ..*settings };
    let depth = self.gain.update(dt, chat_ms, game_ms, &settings, &self.tuning);
    if ducking {
      self.publish(own);
    }
    let gain = 10f32.powf(-depth / 20.0);
    own.duck_permille.store(((1.0 - gain) * 1000.0).round() as u32, Ordering::Relaxed);
    if gain < 1.0 || self.last_gain < 1.0 {
      self.apply(buf, channels, gain);
    }
    self.last_gain = gain;
  }

  /// What a game stream saw and decided in its last block.
  pub fn status(&self) -> DuckStatus {
    self.gain.status()
  }
}

// ---------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------

impl StreamDucker {
  /// Everything that depends on past audio, in one place (a stream gap, going idle), the lowering
  /// level included.
  fn restart(&mut self) {
    self.state = [[0.0; 2]; MAX_CHANNELS];
    self.gain.restart();
    self.last_gain = 1.0;
  }

  /// Once, when the feature goes off or the channel is lost: the slot reads "none".
  fn go_idle(&mut self, own: &StreamSlot) {
    if !self.idle {
      self.restart();
      own.speech_ms.store(0, Ordering::Relaxed);
      own.duck_permille.store(0, Ordering::Relaxed);
      dbg::clear(own);
      self.idle = true;
    }
  }

  /// The telemetry: plain stores every block.
  fn publish(&self, own: &StreamSlot) {
    dbg::publish(own, &self.status());
  }

  /// Mean square of the band over the block; advances the filter state.
  fn measure(&mut self, buf: &[f32], channels: usize) -> f32 {
    self.band.measure(&mut self.state, buf, channels)
  }

  /// `y = g x`, `g` ramped per frame from the last block's gain to `gain`.
  fn apply(&self, buf: &mut [f32], channels: usize, gain: f32) {
    let frames = (buf.len() / channels) as f32;
    let step = (gain - self.last_gain) / frames;
    for (n, frame) in buf.chunks_exact_mut(channels).enumerate() {
      let g = self.last_gain + step * (n + 1) as f32;
      frame.iter_mut().for_each(|x| *x *= g);
    }
  }
}

/// A Game stream played within the last `GAP_HNS`. Without one a chat stream's speech lowers
/// nothing (mix mode works on a playing game stream too), so its voice detector can rest; the
/// detector's restart rule uses the same gap.
pub fn game_playing(table: &StreamTable, now_hns: u64) -> bool {
  table.slots.iter().any(|slot| {
    StreamBus::from_u32(slot.bus.load(Ordering::Relaxed)) == StreamBus::Game
      && now_hns.saturating_sub(slot.last_audio_hns.load(Ordering::Relaxed)) <= GAP_HNS
  })
}

/// The summed speech-band levels of the fresh chat and game slots; the own (game) slot counts with
/// this block's level.
fn levels(table: &StreamTable, own_index: usize, own_ms: f32, now_hns: u64) -> (f32, f32) {
  let (mut chat, mut game) = (0.0f32, own_ms);
  for (index, slot) in table.slots.iter().enumerate() {
    if index == own_index || now_hns.saturating_sub(slot.last_audio_hns.load(Ordering::Relaxed)) > FRESH_HNS {
      continue;
    }
    let ms = f32::from_bits(slot.speech_ms.load(Ordering::Relaxed));
    // Written by another instance: never let a bad value through.
    let ms = if ms.is_finite() && ms > 0.0 { ms } else { 0.0 };
    match StreamBus::from_u32(slot.bus.load(Ordering::Relaxed)) {
      StreamBus::Chat => chat += ms,
      StreamBus::Game => game += ms,
      _ => {}
    }
  }
  (chat, game)
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests;
