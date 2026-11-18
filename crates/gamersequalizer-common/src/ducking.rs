//! Automatic ChatMix ("ducking"): while someone in the voice chat talks over a loud game, the game
//! is lowered in the speech band, by one depth for the whole run of the game, never with a step.
//!
//! Where it runs: every app audio stream has its own stream APO instance, before the mix, with a
//! `StreamDucker` (`stream.rs`). Each instance measures its stream's speech-band level and publishes
//! it in its `StreamSlot`; the service tells each slot its channel (`StreamBus`). A game stream reads
//! the chat streams' levels straight from the shared `StreamTable` (a few atomic loads per block) and
//! lowers itself: no daemon in the loop. A chat stream's level counts only while its voice detector
//! (Silero, run by the APO glue) hears speech, so coughs and room noise never lower the game.
//!
//! The cut: a band-pass around the speech band subtracted from the signal, `x - c * bp(x)`, is a
//! bell of gain `1 - c` at its centre: bass and highs stay, and the depth moves per frame without
//! new coefficients. At depth 0 the buffer is not touched.
//!
//! What it decides (`gain.rs`), per 10 ms block:
//! - Whether, per talk spurt: a spurt is talk with pauses shorter than the hold; its level is a
//!   leaky mean of its talking blocks. The spurt ducks once the current game level masks it (game -
//!   spurt + margin above a threshold) and stays ducked until it ends: a loud talker above the game
//!   does not duck it, a quiet one does, and a quiet talker answering a loud one does within a second.
//! - How much, once for the run: the depth comes from a locked difference between two learned
//!   levels, the loud game level (an upper envelope of the game) and the quiet talker level (the
//!   quietest of the people who normally talk: a mean of the first talkers, then a lower envelope).
//!   Once both are settled the lock is a backlash, so the depth stays the same for the whole run.
//!   Volumes and the user's Strength (the margin) are added live on top.
//! - The depth moves only at slew rates, slowly for the first words of a spurt, and is frozen in the
//!   pauses between words.
//!
//! The service stores the two settled levels per game exe and seeds the next run of that game, so
//! it is locked from its first sentence. Every level and time is a starting point to be tuned by
//! ear and lives in `DuckTuning` (`tuning.rs`), which the service changes while the APO runs; none
//! of them is measured. `dbg.rs` is the per-slot telemetry the service logs.

use crate::shm::ShmHeader;
use crate::streams::StreamBus;
use std::sync::atomic::{AtomicU32, Ordering};

pub mod dbg;
mod gain;
mod stream;
mod tuning;

pub use gain::DuckStatus;
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
  /// Linear volume the listener hears game / chat at, relative to the stream the APO sees.
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
