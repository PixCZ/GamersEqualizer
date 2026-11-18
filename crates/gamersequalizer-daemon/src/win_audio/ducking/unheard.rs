//! A game Automatic ChatMix cannot hear. With spatial sound on, a game that renders spatial objects
//! sends them past the per-app (stream) effects and its session meter (DEVLOG Milestone 19): its
//! stream plays digital silence. Said only on that evidence, as the live run showed it: a paired
//! game stream that plays (fresh) and reads silence for 10 s. A game that plays plain stereo / 7.1
//! into the virtualizer still goes through its stream and works; a paused or alt-tabbed game has no
//! fresh stream and is no case. Known limit: a game with no ordinary stream at all is not seen. The service
//! logs it (`Ducking::tick`) and the window shows it (`AppState::ducking_unheard`), both with this
//! rule and these inputs.

use gamersequalizer_common::ducking::dbg::{self, BAND_DB};
use gamersequalizer_common::streams::{StreamBus, StreamTable};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::view;

/// A game stream's band level at or below this (dB x 10) is digital silence (measured: -120.0).
const SILENT_X10: i32 = -1100;
/// The evidence must hold this long, so a pause in the game's sound or a game starting up is no case.
const HOLD: Duration = Duration::from_secs(10);

/// One look at what decides it.
#[derive(Debug, Clone, Copy)]
pub struct Evidence<'a> {
  /// Automatic ChatMix is on.
  pub enabled: bool,
  /// Spatial sound is on on the default output.
  pub spatial_on: bool,
  /// Band levels (dB x 10) of the game streams that play now (`playing_game_bands`).
  pub game_bands: &'a [i32],
}

#[derive(Debug, Default)]
pub struct UnheardWatch {
  /// Since when the evidence holds.
  since: Option<Instant>,
  unheard: bool,
}

impl UnheardWatch {
  /// Takes one look; true while the game cannot be heard (the evidence held for `HOLD`).
  pub fn observe(&mut self, now: Instant, e: &Evidence) -> bool {
    let silent = |b: &i32| *b != dbg::NA && *b <= SILENT_X10;
    // A game stream plays, and every one that plays reads silence.
    let evidence = e.enabled && e.spatial_on && !e.game_bands.is_empty() && e.game_bands.iter().all(silent);
    if !evidence {
      (self.since, self.unheard) = (None, false);
      return false;
    }
    let since = *self.since.get_or_insert(now);
    self.unheard = now.saturating_duration_since(since) >= HOLD;
    self.unheard
  }

  pub fn unheard(&self) -> bool {
    self.unheard
  }
}

/// Band levels (dB x 10, `dbg::NA` when not reported) of the streams the router put on the Game
/// bus that play now. Free slots and those of an earlier audiodg play no more, so they drop out.
pub fn playing_game_bands(table: &StreamTable, now_hns: u64) -> Vec<i32> {
  let r = Ordering::Relaxed;
  table
    .slots
    .iter()
    .filter(|s| s.owner.load(Ordering::Acquire) != 0 && StreamBus::from_u32(s.bus.load(r)) == StreamBus::Game)
    .filter(|s| view::is_fresh(s.last_audio_hns.load(r), now_hns))
    .map(|s| s.duck_dbg.get(BAND_DB).map_or(dbg::NA, |a| a.load(r)))
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn look(enabled: bool, spatial_on: bool, game_bands: &[i32]) -> Evidence<'_> {
    Evidence { enabled, spatial_on, game_bands }
  }

  /// Feeds the same look once a second for `secs` seconds from `t`; the last verdict.
  fn hold(w: &mut UnheardWatch, t: Instant, secs: u64, e: &Evidence) -> bool {
    (0..=secs).map(|s| w.observe(t + Duration::from_secs(s), e)).last().unwrap()
  }

  #[test]
  fn a_silent_game_stream_under_spatial_sound_is_unheard_after_10_s_and_audio_ends_it() {
    let (mut w, t) = (UnheardWatch::default(), Instant::now());
    let silent = [-1200];
    assert!(!hold(&mut w, t, 9, &look(true, true, &silent)), "not before 10 s");
    assert!(w.observe(t + HOLD, &look(true, true, &silent)));
    assert!(!w.observe(t + HOLD * 2, &look(true, true, &[-500])), "audio again");
    assert!(!w.observe(t + HOLD * 3, &look(true, true, &silent)), "the 10 s start over");
  }

  #[test]
  fn a_paused_or_alt_tabbed_game_without_a_playing_stream_is_no_case() {
    let (mut w, t) = (UnheardWatch::default(), Instant::now());
    assert!(!hold(&mut w, t, 30, &look(true, true, &[])));
  }

  #[test]
  fn never_without_spatial_sound_or_with_the_feature_off_or_with_audio() {
    let silent = [-1200];
    for e in [look(true, false, &silent), look(false, true, &silent), look(true, true, &[-1200, -400])] {
      let (mut w, t) = (UnheardWatch::default(), Instant::now());
      assert!(!hold(&mut w, t, 30, &e), "{e:?}");
    }
    // A band not reported yet is no evidence of silence.
    let (mut w, t) = (UnheardWatch::default(), Instant::now());
    assert!(!hold(&mut w, t, 30, &look(true, true, &[dbg::NA])));
  }
}
