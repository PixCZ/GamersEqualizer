//! A game Automatic ChatMix cannot hear. With spatial sound on, a game that renders spatial objects
//! sends them past the per-app (stream) effects and its session meter (DEVLOG Milestone 19): its
//! stream plays digital silence. Said only on that evidence, as the live run showed it: a paired
//! game stream that plays (fresh) and reads silence for 3 s. A game that plays plain stereo / 7.1
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
/// Short on purpose: until it says so, the stream mode hears nothing of a game on spatial sound and
/// the mix mode does not run, so the first sentences of a talk go unlowered (live test: 10 s).
/// A wrong verdict costs little: the mix mode lowers the whole game for a moment.
const HOLD: Duration = Duration::from_secs(3);
/// Audio on the stream must last this long to end the verdict; longer than `HOLD` (else the mode
/// flipped every minute or two).
const HEARD_HOLD: Duration = Duration::from_secs(10);

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
  /// Since when the game's stream sounds again, while it was unheard.
  heard_since: Option<Instant>,
  unheard: bool,
}

impl UnheardWatch {
  /// Takes one look; true while the game cannot be heard (the evidence held for `HOLD`).
  pub fn observe(&mut self, now: Instant, e: &Evidence) -> bool {
    let silent = |b: &i32| *b != dbg::NA && *b <= SILENT_X10;
    // A game stream plays, and every one that plays reads silence.
    let evidence = e.enabled && e.spatial_on && !e.game_bands.is_empty() && e.game_bands.iter().all(silent);
    if !evidence {
      // The game's stream sounding in a pause or a menu does not mean it is heard again: while it
      // plays, audio must last `HEARD_HOLD` to end the verdict.
      let audible = e.enabled && e.spatial_on && e.game_bands.iter().any(|b| !silent(b) && *b != dbg::NA);
      self.since = None;
      if self.unheard && audible {
        let heard_since = *self.heard_since.get_or_insert(now);
        if now.saturating_duration_since(heard_since) < HEARD_HOLD {
          return true;
        }
      }
      (self.heard_since, self.unheard) = (None, false);
      return false;
    }
    self.heard_since = None;
    let since = *self.since.get_or_insert(now);
    self.unheard |= now.saturating_duration_since(since) >= HOLD;
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
  fn a_silent_game_stream_under_spatial_sound_is_unheard_after_3_s_and_10_s_of_audio_end_it() {
    let (mut w, t) = (UnheardWatch::default(), Instant::now());
    let silent = [-1200];
    assert!(!hold(&mut w, t, 2, &look(true, true, &silent)), "not before 3 s");
    assert!(w.observe(t + HOLD, &look(true, true, &silent)));
    let (a, b) = (t + HEARD_HOLD * 2, t + HEARD_HOLD * 3);
    assert!(w.observe(a, &look(true, true, &[-500])), "a short sound in a pause does not end it");
    assert!(w.observe(a + HEARD_HOLD / 2, &look(true, true, &silent)), "silence again: still unheard");
    assert!(w.observe(b, &look(true, true, &[-500])), "the sound restarts its count");
    assert!(!w.observe(b + HEARD_HOLD, &look(true, true, &[-500])), "audio for 10 s ends it");
    assert!(!w.observe(b + HEARD_HOLD * 2, &look(true, true, &silent)), "the 3 s start over");
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
