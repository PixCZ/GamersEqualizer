//! What the feature reads per tick: the paired chat and game streams from the APO's table (the
//! stream router names them) and the settings from the service's mapping. The status line, the
//! minute summary and the debug log all work from this one walk of the table.

use gamersequalizer_common::ducking::DuckTuning;
use gamersequalizer_common::ducking::dbg::{self, FLAG_VAD, FLAGS};
use gamersequalizer_common::shm::ShmHeader;
use gamersequalizer_common::streams::{DUCK_DBG_SLOTS, StreamTable};
use std::sync::atomic::Ordering;

use super::tuning_file::tuning_text;
use crate::win_audio::stream_router::PairedStream;

/// A stream's values count only while it played this recently (200 ms, QPC 100 ns units): the
/// engine stops calling a silent stream, so its last values would stick.
const FRESH_HNS: u64 = 2_000_000;

/// One paired chat or game stream, read this tick.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamView {
  /// Slot index, and the owner value of the stream in it (a new stream in that slot has another).
  pub index: usize,
  pub owner: u64,
  pub is_game: bool,
  pub exe: String,
  /// The app's image path (empty for mix mode's line).
  pub path: String,
  pub sample_rate: u32,
  pub channels: u32,
  /// It played within the last 200 ms; else its values are old.
  pub fresh: bool,
  /// How far this (game) stream is lowered now, permille of amplitude.
  pub duck_permille: u32,
  /// Speech-band mean square of its last block, before any volume.
  pub speech_ms: f32,
  /// `StreamSlot::duck_dbg`: what the APO saw and decided in its last block.
  pub dbg: [i32; DUCK_DBG_SLOTS],
  /// Mix mode's line (`MixRun::view`): the mix level and the chat band level, dB x 10, and whether
  /// the chat made sound (the game's level held, not measured).
  pub mix: Option<(i32, i32, bool)>,
}

/// The settings the APO works with, as the log reports them.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
  pub strength_permille: u32,
  /// (game, chat) gains, permille.
  pub gains_permille: (u32, u32),
  /// The master switch in the header is on (not bypassed).
  pub master_on: bool,
  /// Non-default tuning, or "defaults".
  pub tuning: String,
  /// A chat sample counts as speech from this detector probability (x 1000, tuning `speech_prob`),
  /// and as sound from this band level (dB x 10, tuning `talk_threshold_db`).
  pub speech_prob_permille: i32,
  pub talk_threshold_db_x10: i32,
}

/// The paired streams as the table holds them now, chat streams first, each kind by owner.
pub fn read_streams(table: &StreamTable, paired: Vec<PairedStream>, now_hns: u64) -> Vec<StreamView> {
  let r = Ordering::Relaxed;
  let mut streams: Vec<StreamView> = paired
    .into_iter()
    .filter_map(|p| {
      let (index, slot) = table.slots.iter().enumerate().find(|(_, s)| s.owner.load(Ordering::Acquire) == p.owner)?;
      let last = slot.last_audio_hns.load(r);
      Some(StreamView {
        index,
        owner: p.owner,
        is_game: p.is_game,
        exe: p.exe,
        path: p.path,
        sample_rate: slot.sample_rate.load(r),
        channels: slot.channels.load(r),
        fresh: is_fresh(last, now_hns),
        duck_permille: slot.duck_permille.load(r),
        speech_ms: f32::from_bits(slot.speech_ms.load(r)),
        dbg: std::array::from_fn(|i| slot.duck_dbg.get(i).map_or(dbg::NA, |a| a.load(r))),
        mix: None,
      })
    })
    .collect();
  streams.sort_by_key(|s| (s.is_game, s.owner));
  streams
}

impl Settings {
  pub fn read(h: &ShmHeader, tuning: &DuckTuning) -> Self {
    let r = Ordering::Relaxed;
    Self {
      strength_permille: h.duck_strength_permille.load(r),
      gains_permille: (h.duck_game_gain_permille.load(r), h.duck_chat_gain_permille.load(r)),
      master_on: !h.master_bypass.load(r),
      tuning: tuning_text(tuning),
      speech_prob_permille: (tuning.speech_prob * 1000.0).round() as i32,
      talk_threshold_db_x10: (tuning.talk_threshold_db * 10.0).round() as i32,
    }
  }
}

/// A stream that last played at `last_audio_hns` still plays (0: never played).
pub fn is_fresh(last_audio_hns: u64, now_hns: u64) -> bool {
  last_audio_hns != 0 && now_hns.saturating_sub(last_audio_hns) < FRESH_HNS
}

/// How far the game is lowered now (permille): the most over the game streams that play.
pub fn reduction(streams: &[StreamView]) -> u32 {
  main_game(streams).map_or(0, |s| s.duck_permille)
}

/// The playing game stream lowered the most; at a tie mix mode's line, else the first one. In mix
/// mode the game's own stream plays silence (its ducker learns nothing useful), so a summary taken
/// while nothing is lowered must show the mix's levels, not that stream's.
pub fn main_game(streams: &[StreamView]) -> Option<&StreamView> {
  let rank = |s: &StreamView| (s.duck_permille, s.mix.is_some());
  streams.iter().filter(|s| s.is_game && s.fresh).reduce(|best, s| if rank(s) > rank(best) { s } else { best })
}

/// One `duck_dbg` value, `dbg::NA` when missing.
pub fn at(d: &[i32; DUCK_DBG_SLOTS], index: usize) -> i32 {
  d.get(index).copied().unwrap_or(dbg::NA)
}

/// A `FLAGS` bit; false while the flags are not reported yet.
pub fn flag(d: &[i32; DUCK_DBG_SLOTS], bit: i32) -> bool {
  let flags = at(d, FLAGS);
  flags != dbg::NA && flags & bit != 0
}

/// A chat stream's voice detector: "yes", "no", or "?" before its flags are reported.
pub fn detector(s: &StreamView) -> &'static str {
  if at(&s.dbg, FLAGS) == dbg::NA {
    "?"
  } else if flag(&s.dbg, FLAG_VAD) {
    "yes"
  } else {
    "no"
  }
}

#[cfg(test)]
pub(super) mod tests {
  use super::*;

  /// A playing stream with every `duck_dbg` value missing.
  pub fn stream(owner: u64, is_game: bool) -> StreamView {
    StreamView {
      index: 0,
      owner,
      is_game,
      exe: if is_game { "game.exe" } else { "Discord.exe" }.to_string(),
      path: String::new(),
      sample_rate: 48_000,
      channels: 2,
      fresh: true,
      duck_permille: 0,
      speech_ms: 0.0,
      dbg: [dbg::NA; DUCK_DBG_SLOTS],
      mix: None,
    }
  }

  #[test]
  fn the_game_is_lowered_by_its_most_lowered_playing_stream() {
    let mut a = stream(5, true);
    a.duck_permille = 300;
    let mut stale = stream(6, true);
    (stale.duck_permille, stale.fresh) = (800, false);
    let mut chat = stream(1, false);
    chat.duck_permille = 900; // only game streams are lowered
    let streams = vec![chat, a, stale];
    assert_eq!(reduction(&streams), 300);
    assert_eq!(main_game(&streams).map(|s| s.owner), Some(5));
    assert_eq!(reduction(&[]), 0);
  }

  #[test]
  fn with_nothing_lowered_mix_modes_line_is_the_game() {
    // Regression: the minute summary of mix mode showed the silent game stream's levels
    // (live log 2026-11-19: loud level -54.7 dB in the summary, -43.7 dB in the mix lines).
    let (silent, mut mix) = (stream(5, true), stream(u64::MAX, true));
    mix.mix = Some((-500, -600, false));
    let streams = vec![silent.clone(), mix.clone()];
    assert_eq!(main_game(&streams).map(|s| s.owner), Some(u64::MAX));
    let mut lowered = silent;
    lowered.duck_permille = 100; // a stream that is lowered still wins
    assert_eq!(main_game(&[mix, lowered]).map(|s| s.owner), Some(5));
  }

  #[test]
  fn the_detector_is_unknown_until_flags_arrive() {
    let mut chat = stream(1, false);
    assert_eq!(detector(&chat), "?");
    chat.dbg[FLAGS] = 0;
    assert_eq!(detector(&chat), "no");
    chat.dbg[FLAGS] = FLAG_VAD;
    assert_eq!(detector(&chat), "yes");
  }
}
