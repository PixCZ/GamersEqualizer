//! Automatic ChatMix ("ducking"): while someone in the voice chat talks over a loud game, the game
//! is lowered, by one level that is learned from the fights, never with a step.
//!
//! Where it runs: every app audio stream has its own stream APO instance, before the mix, with a
//! `StreamDucker` (`stream.rs`). Each instance measures its stream's speech-band level and publishes
//! it in its `StreamSlot`; the service tells each slot its channel (`StreamBus`). A game stream reads
//! the chat streams' levels straight from the shared `StreamTable` (a few atomic loads per block) and
//! lowers itself, full band: no daemon in the loop. A chat stream's level counts only while its voice
//! detector (Silero, run by the APO glue) hears speech, so coughs and room noise never lower the game.
//!
//! What it decides (`gain.rs`), per 10 ms block, between two levels:
//! - Nobody talks (a talk spurt is talk with pauses shorter than the hold): nothing is lowered.
//! - Someone talks: the game is lowered by the lowering level. It starts at 0 with every run (nothing
//!   is stored); a spurt the current game level masks (game - spurt level + margin above a
//!   threshold) raises it to what that spurt needs, so it grows to what the loudest fight needed and
//!   never more. It comes down only by 1 dB in 10 minutes (inaudible, `LEVEL_DECAY_DB_PER_S`); it starts over at 0 after a long
//!   silence of the game, a stream gap, the feature turned off, a new strength and any volume change
//!   (a channel output's in the settings, a session volume through `StreamSlot::duck_volume_epoch`).
//! - The depth moves only at slew rates, slowly for the first words of a spurt, and is held in the
//!   pauses between words.
//!
//! Every level and time is a starting point to be tuned by ear and lives in `DuckTuning`
//! (`tuning.rs`), which the service changes while the APO runs; none of them is measured. `dbg.rs`
//! is the per-slot telemetry the service logs.
//!
//! A game whose stream the APO cannot see (spatial audio objects bypass the per-stream effects) is
//! ducked by the service instead (`mix.rs`): the same decision, with the game measured in the
//! output mix (while the chat sounds, with the chat's level taken out: it can only raise the game's
//! level), applied to the game's session volume, full band. `band.rs` is the filter both paths measure with.

use crate::shm::ShmHeader;
use crate::streams::StreamBus;
use std::sync::atomic::{AtomicU32, Ordering};

mod band;
pub mod dbg;
mod gain;
mod mix;
mod stream;
mod tuning;

pub use gain::DuckStatus;
pub use mix::{MixDucker, chat_levels, chat_sounding};
pub use stream::StreamDucker;
pub use tuning::DuckTuning;

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// No block for longer than this (100 ns units): the stream restarted, what depends on past audio
/// starts over (the APO restarts its voice detector by the same rule).
pub const GAP_HNS: u64 = 5_000_000;
/// Mean square read as silence by the dB conversion (-120 dB).
const FLOOR_MS: f32 = 1e-12;
/// A slot whose last block is older than this (100 ns units) is silent: the audio engine does not
/// call a silent stream.
const FRESH_HNS: u64 = 500_000;

// ---------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------

/// What a stream does in Automatic ChatMix, from its `StreamBus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuckRole {
  /// Chat: measures and publishes its speech level, never changed.
  Voice,
  /// Game: lowered in the speech band while the chat talks over it.
  Ducked,
  /// Anything else, unpaired included: untouched.
  Other,
}

/// The user's settings from shared memory, as the DSP uses them (`read` clamps).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DuckSettings {
  pub enabled: bool,
  /// Strength, 0..=1: how far the voice should sit above the game in the band, as a share of
  /// `DuckTuning::max_margin_db` (0.5 = 6 dB by default).
  pub strength: f32,
  /// Linear volume the listener hears game / chat at, relative to the stream the APO sees: only a
  /// channel output's volume (after the APO); session volumes are already in the stream.
  pub game_gain: f32,
  pub chat_gain: f32,
}

// ---------------------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------------------

impl DuckRole {
  pub fn from_bus(bus: StreamBus) -> Self {
    match bus {
      StreamBus::Chat => DuckRole::Voice,
      StreamBus::Game => DuckRole::Ducked,
      _ => DuckRole::Other,
    }
  }
}

impl DuckSettings {
  pub fn read(h: &ShmHeader) -> Self {
    let permille = |a: &AtomicU32| a.load(Ordering::Relaxed) as f32 / 1000.0;
    DuckSettings {
      enabled: h.duck_enabled.load(Ordering::Relaxed),
      strength: permille(&h.duck_strength_permille).min(1.0),
      game_gain: permille(&h.duck_game_gain_permille),
      chat_gain: permille(&h.duck_chat_gain_permille),
    }
  }
}

// ---------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------

/// A mean square in dB, silence floored at -120 dB.
fn db(ms: f32) -> f32 {
  10.0 * ms.max(FLOOR_MS).log10()
}
