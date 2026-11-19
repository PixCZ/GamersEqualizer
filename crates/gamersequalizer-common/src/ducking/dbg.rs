//! Per-slot debug telemetry (`StreamSlot::duck_dbg`), APO -> service, so one evening's log shows
//! what each stream's ducker saw and decided, also when it did nothing. The indices are part of
//! the shared layout: append only, never reorder. Levels are raw (pre-volume) dB x 10.

use super::DuckStatus;
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
/// Game streams from here on: the current game level the decision uses.
pub const GAME_DB: usize = 2;
/// The summed chat speech level this block, after the detectors' gating.
pub const VOICE_SUM_DB: usize = 3;
pub const SPURT_DB: usize = 4;
/// Current game - spurt level + margin, with the volumes applied.
pub const NEED_DB: usize = 5;
/// The depth applied now.
pub const DEPTH_DB: usize = 6;
// 7, 8, 10, 11: retired (target depth, locked difference, fight heard, talk learned of the design
// before the lowering level); an APO built before may still write them, nothing reads them.
/// `FLAG_*` bits.
pub const FLAGS: usize = 9;
/// The lowering level (the depth while someone talks), and how many spurts raised it since it last
/// started over.
pub const LEVEL_DB: usize = 12;
pub const LEVEL_RAISES: usize = 13;

pub const FLAG_TALKING: i32 = 1;
/// This spurt is lowered (an APO built before sets it for a latched spurt, much the same).
pub const FLAG_LOWERING: i32 = 2;
// 4, 8, 16, 32: retired (settled and seeded levels), as the indices above.
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

/// A raw value (flags, a count, probability x 1000).
pub fn set(slot: &StreamSlot, index: usize, value: i32) {
  if let Some(cell) = slot.duck_dbg.get(index) {
    cell.store(value, Ordering::Relaxed);
  }
}

/// A game stream's decision as (index, value) pairs: what `publish` writes into its slot, and what
/// the service's mix mode logs as one.
pub fn of_status(st: &DuckStatus) -> [(usize, i32); 8] {
  let x10 = |db: f32| (db * 10.0).round() as i32;
  let level = |v: Option<f32>| v.map_or(NA, x10);
  let flags = [(st.talking, FLAG_TALKING), (st.lowering, FLAG_LOWERING)]
    .iter()
    .filter(|(on, _)| *on)
    .fold(0, |bits, (_, flag)| bits | flag);
  [
    (GAME_DB, x10(st.game_db)),
    (VOICE_SUM_DB, x10(st.chat_db)),
    (SPURT_DB, level(st.spurt_db)),
    (NEED_DB, level(st.need_db)),
    (DEPTH_DB, x10(st.depth_db)),
    (FLAGS, flags),
    (LEVEL_DB, x10(st.level_db)),
    (LEVEL_RAISES, st.raises.min(i32::MAX as u32) as i32),
  ]
}

/// `of_status` into the slot.
pub fn publish(slot: &StreamSlot, st: &DuckStatus) {
  of_status(st).iter().for_each(|&(index, value)| set(slot, index, value));
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
