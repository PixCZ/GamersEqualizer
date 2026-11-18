//! Per-slot debug telemetry (`StreamSlot::duck_dbg`), APO -> service, so one evening's log shows
//! what each stream's ducker saw and decided, also when it did nothing. The indices are part of
//! the shared layout: append only, never reorder. Levels are raw (pre-volume) dB x 10.

use crate::streams::StreamSlot;
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// This block's speech-band level, every stream while the feature is on (a chat stream also when
/// its detector hears no speech: the log tells "chat silent" from "chat loud but not speech").
pub const BAND_DB: usize = 0;
/// Chat streams: the detector's last chunk, probability x 1000; -1 = no detector.
pub const VAD_PROB: usize = 1;
/// Game streams from here to `QUIET_EVIDENCE_S`: the current game level the decision uses.
pub const GAME_DB: usize = 2;
/// The summed chat speech level this block, after the detectors' gating.
pub const VOICE_SUM_DB: usize = 3;
pub const SPURT_DB: usize = 4;
/// Current game - spurt level + margin, with the volumes applied.
pub const NEED_DB: usize = 5;
/// The depth applied now, and the depth the lock asks for when ducking.
pub const DEPTH_DB: usize = 6;
pub const TARGET_DEPTH_DB: usize = 7;
pub const LOCKED_DIFF_DB: usize = 8;
/// `FLAG_*` bits.
pub const FLAGS: usize = 9;
/// Fight heard and talk learned, whole seconds.
pub const LOUD_EVIDENCE_S: usize = 10;
pub const QUIET_EVIDENCE_S: usize = 11;

pub const FLAG_TALKING: i32 = 1;
pub const FLAG_LATCHED: i32 = 2;
pub const FLAG_LOUD_SETTLED: i32 = 4;
pub const FLAG_QUIET_SETTLED: i32 = 8;
/// A saved level of an earlier run was adopted (not set when it was refused).
pub const FLAG_LOUD_SEEDED: i32 = 16;
pub const FLAG_QUIET_SEEDED: i32 = 32;
/// A voice detector runs on this (chat) stream.
pub const FLAG_VAD: i32 = 64;
/// The stream's processing panicked: it passes audio through, everything else reads `NA`.
pub const FLAG_FAILED: i32 = 128;
/// No value: nothing learned yet, not this stream's role, no spurt, feature off. `StreamSlot::reset`
/// fills the telemetry with it, so a fresh slot never reads as real values.
pub const NA: i32 = i32::MIN;

// ---------------------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------------------

/// A level, dB x 10; None = `NA`.
pub fn store(slot: &StreamSlot, index: usize, db: Option<f32>) {
  set(slot, index, db.map_or(NA, |v| (v * 10.0).round() as i32));
}

/// A raw value (flags, seconds, probability x 1000).
pub fn set(slot: &StreamSlot, index: usize, value: i32) {
  if let Some(cell) = slot.duck_dbg.get(index) {
    cell.store(value, Ordering::Relaxed);
  }
}

/// Everything to `NA` (feature off, a fresh slot).
pub fn clear(slot: &StreamSlot) {
  slot.duck_dbg.iter().for_each(|cell| cell.store(NA, Ordering::Relaxed));
}

/// A stream whose processing panicked: the APO passes its audio through from now on. Its slot
/// stays fresh (`record` goes on), so its last speech level would keep every game ducked: zero
/// it, and leave only `FLAG_FAILED` in the telemetry so the log says why.
pub fn fail(slot: &StreamSlot) {
  slot.speech_ms.store(0, Ordering::Relaxed);
  slot.duck_permille.store(0, Ordering::Relaxed);
  clear(slot);
  set(slot, FLAGS, FLAG_FAILED);
}
