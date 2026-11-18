//! The ducker's tuning set: every value to be tuned by ear, carried through shared memory
//! (`ShmHeader::duck_tune`) so the service can change it while the APO runs. One APO install must
//! last through all tuning, so every tunable lives here, not in a `const`.

use crate::shm::{DUCK_TUNE_SLOTS, ShmHeader};
use std::sync::atomic::Ordering;

// ---------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------

/// One table ties each value's name, slot, default and sane range together, so `set`,
/// `non_default`, `write_to` and `read_from` cannot disagree.
macro_rules! duck_tuning {
  ($( $(#[doc = $doc:literal])* $name:ident = $default:literal in $min:literal ..= $max:literal, )*) => {
    /// Every tunable of the ducker. `Default` is the starting point; nothing in it is measured.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct DuckTuning {
      $( $(#[doc = $doc])* pub $name: f32, )*
    }

    /// (name, default, min, max). The position is the `ShmHeader::duck_tune` slot, part of the
    /// shared-memory layout: append only, never reorder or remove.
    pub(super) const PARAMS: &[(&str, f32, f32, f32)] = &[ $( (stringify!($name), $default, $min, $max), )* ];

    impl DuckTuning {
      pub const DEFAULT: DuckTuning = DuckTuning { $( $name: $default, )* };

      fn values(&self) -> [f32; PARAMS.len()] {
        [ $( self.$name, )* ]
      }

      fn values_mut(&mut self) -> [&mut f32; PARAMS.len()] {
        [ $( &mut self.$name, )* ]
      }
    }
  };
}

duck_tuning! {
  /// Speech band: a band-pass around 1.5 kHz with Q 0.5 covers about 500 Hz .. 4.5 kHz, the band
  /// that carries intelligibility. A change while ducking reshapes the cut at once (audible): make
  /// it between sentences.
  band_center_hz = 1500.0 in 300.0..=4000.0,
  band_q = 0.5 in 0.2..=3.0,
  /// Raw chat speech level (band mean square, dB) above which someone talks.
  talk_threshold_db = -50.0 in -90.0..=-10.0,
  /// A chat stream's voice detector (Silero VAD, run by the APO) hears speech in a 32 ms chunk at
  /// or above this probability. DEVLOG milestone 38: at 0.5 normal speech is found in 90-98 % of
  /// loud frames, pause frames score 0.4-8 %, keyboard 0 % (clicks never exceed 0.15).
  speech_prob = 0.5 in 0.05..=0.95,
  /// The depth holds this long after the last talking block, over pauses between words.
  hold_s = 0.5 in 0.05..=5.0,
  /// A spurt's level (the mean dB of its talking blocks) decides whether it ducks only from this
  /// much talk on, so a soft syllable onset cannot latch a loud talker's spurt.
  spurt_min_s = 0.2 in 0.0..=2.0,
  /// A spurt teaches the quiet talker level only from this much talk on: a lone "ok" or a cough
  /// that slipped past the voice detector teaches nothing.
  spurt_learn_s = 1.0 in 0.1..=10.0,
  /// The spurt level is a leaky mean over this much talk (a plain mean until then), so a quiet
  /// talker answering a loud one within the hold is heard at their own level within about this.
  spurt_level_s = 1.0 in 0.05..=30.0,
  /// Talk time of a spurt under which the depth grows only slowly: a single word causes a small
  /// dip only.
  short_talk_s = 0.35 in 0.0..=5.0,
  /// The only rates the depth moves at (the "no sharp transition").
  slow_attack_db_per_s = 8.0 in 0.5..=200.0,
  fast_attack_db_per_s = 40.0 in 0.5..=200.0,
  release_db_per_s = 12.0 in 0.5..=200.0,
  /// Time constants of the current game level (rise, fall), which decides whether a spurt ducks:
  /// a click in a calm game must barely move it, sustained gunfire must.
  game_rise_s = 0.4 in 0.01..=10.0,
  game_fall_s = 1.5 in 0.01..=30.0,
  /// The learned loud game level is an upper envelope of the current game level: above it, it
  /// rises with a time constant (dB) of `loud_rise_s` plus one second per `loud_slowdown` seconds
  /// of fight heard, at most `loud_rise_max_s`: fast in the first fight, then one explosion moves
  /// it less and less. Fight heard = the current game level would mask the quiet talker (or,
  /// before one is learned, the current spurt); nothing counts before a voice is heard, so calm
  /// play and menus before the first fight leave the rise fast.
  loud_rise_s = 3.0 in 0.1..=60.0,
  loud_rise_max_s = 20.0 in 0.1..=300.0,
  loud_slowdown = 15.0 in 1.0..=1000.0,
  /// Otherwise it falls at this fixed rate per minute of processed audio, however far below the
  /// game is, and never below the current level: a minute of travel costs 0.5 dB, ten minutes
  /// 5 dB; a one-off extreme and fights that really got quieter are forgotten over tens of
  /// minutes, the depth staying within `max_depth_db` meanwhile.
  loud_fall_db_per_min = 0.5 in 0.0..=60.0,
  /// A loud level remembered from an earlier run counts as this much fight heard.
  loud_warm_s = 30.0 in 0.0..=600.0,
  /// The quiet talker level is a lower envelope over spurt levels. A quieter spurt pulls it down
  /// with a time constant (in seconds of the spurt's talk) of `quiet_fall_s` plus one second per
  /// `quiet_slowdown` seconds of talk learned, at most `quiet_fall_max_s`; once settled, a spurt
  /// counts as at most `quiet_outlier_db` under it (one whisper or a far-away mic moves it a
  /// little, a real quiet talker gets there over a few spurts). A louder spurt lets it rise only
  /// at `quiet_rise_db_per_min` per minute of talk: a quiet person who left is forgotten.
  /// The fall is fast on purpose: the quiet person usually talks little, so the level must get
  /// there within a few of their sentences. Cost: one whispered stretch of about 3 s moves the
  /// depth by roughly 2.5 dB, and it comes back only at the slow rise.
  quiet_fall_s = 2.0 in 0.1..=60.0,
  quiet_fall_max_s = 3.0 in 0.1..=600.0,
  quiet_slowdown = 10.0 in 1.0..=1000.0,
  quiet_outlier_db = 6.0 in 0.0..=60.0,
  quiet_rise_db_per_min = 1.0 in 0.0..=60.0,
  /// A quiet talker level remembered from an earlier run counts as this much talk (so as settled).
  quiet_warm_s = 10.0 in 0.0..=600.0,
  /// The depth comes from a locked loud - quiet talker difference. Until the loud level has this
  /// much fight heard (so: 20 s of masking game after the first voice was heard) and the quiet
  /// talker level this much talk learned, the lock follows the estimates; a remembered pair
  /// counts at once.
  loud_settled_s = 20.0 in 0.0..=600.0,
  quiet_settled_s = 10.0 in 0.0..=600.0,
  /// Then the lock is a backlash: the estimated difference must rise `hysteresis_up_db` above it
  /// before the depth grows, and fall `hysteresis_down_db` below it before the depth shrinks; a
  /// larger shift moves the lock by the excess only, never with a step. Buys one depth for the
  /// whole run; costs that a real change within the band is ignored. Asymmetric because a depth
  /// that is too shallow fails (the voice is not understood) while one too deep only makes the
  /// game a little quieter than needed: small upwards, so the lock never stays short of the
  /// estimate while the quiet talker level still falls. Both must stay clearly below the margin
  /// (6 dB at the default strength), or the lock could leave the voice no margin at all.
  hysteresis_up_db = 0.5 in 0.0..=20.0,
  hysteresis_down_db = 4.0 in 0.0..=20.0,
  /// A spurt ducks once the current game level would need more than this (game - spurt level +
  /// margin).
  on_threshold_db = 2.0 in -20.0..=20.0,
  /// The voice's margin over the game at full strength (`duck_strength_permille` 1000; 500 = half).
  max_margin_db = 12.0 in 0.0..=30.0,
  /// Safety bound of the depth, whatever the levels and the strength (20 dB: the game's speech
  /// band to one tenth of its amplitude).
  max_depth_db = 20.0 in 0.0..=40.0,
  /// A ducked spurt lets go once the game has not masked the talker for this long in a row, and
  /// may duck again later in the same spurt: a talker who never pauses (a podcast) is not ducked
  /// for minutes by one loud moment. Gaps in a fight shorter than this do not pump.
  latch_release_s = 3.0 in 0.5..=600.0,
  /// The service's mix ducker (`MixDucker`) measures the game only this long after the chat last
  /// made a sound: it covers the chat's way through the spatial renderer and the tail of a word.
  mix_chat_guard_s = 0.3 in 0.05..=5.0,
  /// The mix ducker lowers the game's whole band (its session volume), so it needs less than the
  /// band cut's `max_depth_db`.
  mix_max_depth_db = 12.0 in 0.0..=40.0,
}

const _: () = assert!(PARAMS.len() <= DUCK_TUNE_SLOTS, "more tunables than shared-memory slots");

// ---------------------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------------------

impl Default for DuckTuning {
  fn default() -> Self {
    Self::DEFAULT
  }
}

impl DuckTuning {
  /// Sets the value named `name` (the field's name). False, and nothing changed, for an unknown
  /// name, a non-finite value or one outside that value's sane range.
  pub fn set(&mut self, name: &str, value: f32) -> bool {
    let Some(index) = PARAMS.iter().position(|p| p.0 == name) else { return false };
    let valid = PARAMS.get(index).is_some_and(|p| in_range(p, value));
    match self.values_mut().into_iter().nth(index) {
      Some(field) if valid => {
        *field = value;
        true
      }
      _ => false,
    }
  }

  /// The values that differ from the defaults, for the log.
  pub fn non_default(&self) -> Vec<(&'static str, f32)> {
    PARAMS.iter().zip(self.values()).filter(|(p, v)| *v != p.1).map(|(p, v)| (p.0, v)).collect()
  }

  /// Publishes all values; `duck_tune_version` moves only when one changed.
  pub fn write_to(&self, h: &ShmHeader) {
    let mut changed = false;
    for (slot, value) in h.duck_tune.iter().zip(self.values()) {
      changed |= slot.swap(value.to_bits(), Ordering::Relaxed) != value.to_bits();
    }
    if changed {
      h.duck_tune_version.fetch_add(1, Ordering::Release);
    }
  }

  /// What is published, never trusting the other process: a value that is not finite or out of
  /// its range reads as its default.
  pub fn read_from(h: &ShmHeader) -> Self {
    let mut tuning = Self::DEFAULT;
    for ((slot, param), field) in h.duck_tune.iter().zip(PARAMS).zip(tuning.values_mut()) {
      let value = f32::from_bits(slot.load(Ordering::Relaxed));
      if in_range(param, value) {
        *field = value;
      }
    }
    tuning
  }
}

// ---------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------

fn in_range(&(_, _, min, max): &(&str, f32, f32, f32), value: f32) -> bool {
  value.is_finite() && (min..=max).contains(&value)
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use crate::shm::CopiedEqVersions;

  const T: DuckTuning = DuckTuning::DEFAULT;

  #[test]
  fn tuning_round_trips_through_the_header_and_the_mirror() {
    let (daemon, apo) = (ShmHeader::default(), ShmHeader::default());
    let version = |h: &ShmHeader| h.duck_tune_version.load(Ordering::Relaxed);
    assert_eq!(DuckTuning::read_from(&daemon), T);
    let mut tuning = T;
    assert!(tuning.set("hold_s", 0.8) && tuning.set("hysteresis_up_db", 1.0));
    assert_eq!(tuning.non_default(), vec![("hold_s", 0.8), ("hysteresis_up_db", 1.0)]);
    let before = version(&daemon);
    tuning.write_to(&daemon);
    tuning.write_to(&daemon);
    assert_eq!((version(&daemon), DuckTuning::read_from(&daemon)), (before + 1, tuning), "bumped once");
    let before = version(&apo);
    apo.copy_controls_from(&daemon, &mut CopiedEqVersions::default());
    apo.copy_controls_from(&daemon, &mut CopiedEqVersions::default());
    assert_eq!((version(&apo), DuckTuning::read_from(&apo)), (before + 1, tuning));
  }

  #[test]
  fn tuning_rejects_unknown_names_and_bad_values() {
    let mut tuning = T;
    assert!(!tuning.set("no_such_value", 1.0) && !tuning.set("hold_s", f32::NAN) && !tuning.set("hold_s", 1e6));
    assert_eq!(tuning, T);
    let h = ShmHeader::default();
    for garbage in [f32::NAN, f32::INFINITY, 1e9, -1e9] {
      h.duck_tune.iter().for_each(|slot| slot.store(garbage.to_bits(), Ordering::Relaxed));
      assert_eq!(DuckTuning::read_from(&h), T, "{garbage}");
    }
  }

  #[test]
  fn the_tuning_slots_are_pinned() {
    // The installed APO reads `ShmHeader::duck_tune` by position while the exe is rebuilt: changing
    // this list (other than appending) means a new APO for everyone. Slots 32-34 were added after
    // the APO the user has installed and no APO build reads them, so slot 33 (`mix_chat_factor`)
    // became `mix_chat_guard_s` in place; from here on, only append.
    let names: Vec<&str> = PARAMS.iter().map(|p| p.0).collect();
    let pinned = [
      "band_center_hz",
      "band_q",
      "talk_threshold_db",
      "speech_prob",
      "hold_s",
      "spurt_min_s",
      "spurt_learn_s",
      "spurt_level_s",
      "short_talk_s",
      "slow_attack_db_per_s",
      "fast_attack_db_per_s",
      "release_db_per_s",
      "game_rise_s",
      "game_fall_s",
      "loud_rise_s",
      "loud_rise_max_s",
      "loud_slowdown",
      "loud_fall_db_per_min",
      "loud_warm_s",
      "quiet_fall_s",
      "quiet_fall_max_s",
      "quiet_slowdown",
      "quiet_outlier_db",
      "quiet_rise_db_per_min",
      "quiet_warm_s",
      "loud_settled_s",
      "quiet_settled_s",
      "hysteresis_up_db",
      "hysteresis_down_db",
      "on_threshold_db",
      "max_margin_db",
      "max_depth_db",
      "latch_release_s",
      "mix_chat_guard_s",
      "mix_max_depth_db",
    ];
    assert_eq!(names, pinned);
  }
}
