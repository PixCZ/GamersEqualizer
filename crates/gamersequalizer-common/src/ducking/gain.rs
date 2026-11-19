//! The gain computer of one game stream: from the game's and the chat's speech-band levels, per
//! block, whether this talk spurt ducks and how deep. No filters, no shared memory.
//!
//! All levels are as the APO sees them. A per-app session volume (and so the ChatMix balance) is
//! applied before the APO (DEVLOG Milestone 19), so it is already in them; only the volume of a
//! channel on its own output comes after the APO and is added (`game_gain` / `chat_gain`), with
//! the margin, only where a decision is made.

use super::{DuckSettings, DuckTuning, db};

// ---------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------

/// What a game stream's ducker saw and decided in its last block (for the telemetry and
/// `duck_sim`). Levels are raw dB.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DuckStatus {
  pub talking: bool,
  pub latched: bool,
  pub game_db: f32,
  /// The chat streams' summed speech level this block.
  pub chat_db: f32,
  pub spurt_db: Option<f32>,
  pub spurt_talk_s: f32,
  pub need_db: Option<f32>,
  pub depth_db: f32,
  pub target_db: Option<f32>,
  pub lock_db: Option<f32>,
  pub loud_db: Option<f32>,
  pub quiet_db: Option<f32>,
  pub loud_s: f32,
  pub quiet_s: f32,
  pub loud_settled: bool,
  pub quiet_settled: bool,
  /// Set by `StreamDucker`, which reads the saved levels.
  pub loud_seeded: bool,
  pub quiet_seeded: bool,
}

/// The decision state of one stream. Everything but `learned` depends on past audio and starts
/// over on a restart (`restart`).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct DuckGain {
  depth_db: f32,
  /// Current game level, mean square.
  game_ms: f32,
  /// Hold left after the last talking block, seconds: pauses shorter than this stay in the spurt.
  hold_s: f32,
  /// Talk time of this spurt and its level (dB, a leaky mean over `spurt_level_s`).
  talk_s: f32,
  spurt_db: f32,
  /// The talk ducks: set once the game masked a spurt, until the game has not masked the talk for
  /// `latch_release_s` (a talker who never pauses, e.g. a podcast). Kept between spurts.
  latched: bool,
  /// How long in a row the game has not masked this spurt, seconds.
  unmasked_s: f32,
  /// The last block's inputs and decision, for `DuckStatus`.
  talking: bool,
  chat_db: f32,
  need_db: Option<f32>,
  target_db: Option<f32>,
  learned: Learned,
}

/// What is learned about this stream: lives as long as the stream, across restarts (a game pauses
/// its stream in menus and on alt-tab) and the feature turned off. Its settled levels are stored
/// per game exe and seed the next run (`seed_*`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Learned {
  /// Loud game level, dB (None = nothing heard yet), and the fight heard, seconds.
  loud_db: Option<f32>,
  loud_s: f32,
  /// Quiet talker level, dB (None = nothing learned yet), and the talk it learned from, seconds.
  quiet_db: Option<f32>,
  quiet_s: f32,
  /// The locked loud - quiet difference the depth is built from, dB (None = no voice yet). Not
  /// stored: a next run locks onto the stored levels' difference, up to the backlash away.
  lock_db: Option<f32>,
}

// ---------------------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------------------

impl DuckGain {
  /// One block of `dt` seconds: `chat_ms` / `game_ms` are the summed raw speech-band levels of the
  /// chat and the game streams. `loud_dt`: seconds of game `game_ms` stands for in the loud level,
  /// 0 = it teaches nothing (the mix ducker measures only in the chat's pauses, and one
  /// measurement stands for the time since the previous one). Returns the depth (dB) the game's
  /// band is lowered by.
  pub(super) fn update(
    &mut self,
    dt: f32,
    chat_ms: f32,
    game_ms: f32,
    loud_dt: f32,
    s: &DuckSettings,
    t: &DuckTuning,
  ) -> f32 {
    // The game level runs while nobody talks too, so it is known when speech starts.
    self.game_ms = smooth(self.game_ms, game_ms, dt, t.game_rise_s, t.game_fall_s);
    let game_db = db(self.game_ms);
    if loud_dt > 0.0 {
      self.learned.learn_loud(game_db, loud_dt, t);
    }

    let talking = s.enabled && s.chat_gain > 0.0 && db(chat_ms) > t.talk_threshold_db;
    let talked_s = self.talk_s;
    if talking {
      self.talk_s += dt;
      // Leaky, so a quiet talker answering a loud one within the hold is heard at their own level
      // within about `spurt_level_s`.
      let weight = if self.talk_s < t.spurt_level_s { dt / self.talk_s } else { 1.0 - (-dt / t.spurt_level_s).exp() };
      self.spurt_db += (db(chat_ms) - self.spurt_db) * weight;
      if self.talk_s >= t.spurt_learn_s {
        self.learned.learn_quiet(self.spurt_db, dt, t);
      }
      self.hold_s = t.hold_s;
    } else {
      self.hold_s = if s.enabled { (self.hold_s - dt).max(0.0) } else { 0.0 };
      if self.hold_s <= 0.0 {
        (self.talk_s, self.spurt_db) = (0.0, 0.0);
        // The latch outlives the talk: the next talk starts where this one ended (lowered or not)
        // instead of measuring the fight again (mix mode measures it only in the chat's pauses).
        // shortcut: talk an hour later in a lobby is lowered for `latch_release_s` first; forget
        // after a long silence if that bothers. The calm count carries on too: sentences of a few
        // seconds in a calm game add up to the release (a fight resets it at its first block).
        if !s.enabled {
          (self.latched, self.unmasked_s) = (false, 0.0);
        }
      }
    }
    let spurt_db = (self.talk_s > 0.0).then_some(self.spurt_db);

    // What the listener hears: the volumes after the APO (channel outputs) and the margin.
    let gains_db = db(s.game_gain * s.game_gain) - db(s.chat_gain * s.chat_gain);
    let margin_db = s.strength * t.max_margin_db;
    let need_db = |talker_db: f32| game_db - talker_db + gains_db + margin_db;
    // Before a quiet talker is learned, the current spurt stands in for it.
    let quiet_db = self.learned.quiet_db.or(spurt_db);
    if quiet_db.is_some_and(|quiet| need_db(quiet) > t.on_threshold_db) {
      self.learned.loud_s += dt; // fight heard
    }
    self.learned.update_lock(quiet_db, t);
    if let Some(spurt) = spurt_db {
      let masks = need_db(spurt) > t.on_threshold_db;
      // Decided only from `spurt_min_s` on, so a soft onset neither latches nor resets the calm count.
      if self.talk_s >= t.spurt_min_s {
        self.latched |= masks && talking;
        // Lets go only after the game stopped masking it for a while: gaps in a fight do not pump.
        self.unmasked_s = if masks { 0.0 } else { self.unmasked_s + dt };
        if self.unmasked_s >= t.latch_release_s {
          self.latched = false;
        }
      }
    }
    self.target_db = self.learned.lock_db.map(|lock| (lock + gains_db + margin_db).clamp(0.0, t.max_depth_db));
    (self.talking, self.chat_db, self.need_db) = (talking, db(chat_ms), spurt_db.map(need_db));

    let goal_db = if !self.latched || spurt_db.is_none() {
      0.0 // between spurts a kept latch lowers nothing: nobody to hear over the game
    } else if talking {
      self.target_db.unwrap_or(0.0)
    } else {
      self.depth_db // a pause between words: frozen
    };
    let attack = if talked_s < t.short_talk_s { t.slow_attack_db_per_s } else { t.fast_attack_db_per_s };
    self.depth_db = if goal_db > self.depth_db {
      (self.depth_db + attack * dt).min(goal_db)
    } else {
      (self.depth_db - t.release_db_per_s * dt).max(goal_db)
    };
    self.depth_db
  }

  pub(super) fn depth_db(&self) -> f32 {
    self.depth_db
  }

  /// A stream gap or going idle: everything that depends on past audio starts over, what was
  /// learned stays.
  pub(super) fn restart(&mut self) {
    *self = DuckGain { learned: self.learned, ..DuckGain::default() };
  }

  /// A volume before the measurement moved (a session volume, `game_db` / `chat_db` its change):
  /// the learned levels and the current game level move with it, so the next fight gets the
  /// depth for the new volume at once instead of after tens of minutes of slow learning.
  pub(super) fn shift(&mut self, game_db: f32, chat_db: f32) {
    let l = &mut self.learned;
    l.loud_db = l.loud_db.map(|v| v + game_db);
    l.quiet_db = l.quiet_db.map(|v| v + chat_db);
    l.lock_db = l.lock_db.map(|v| v + game_db - chat_db);
    self.game_ms *= 10f32.powf(game_db / 10.0);
    self.spurt_db += chat_db;
  }

  /// Adopts a level an earlier run of this game learned, unless this stream's own estimate has
  /// already settled (a service restart or a late pairing mid-run must not shift the depth).
  /// Returns whether it was adopted.
  pub(super) fn seed_loud(&mut self, db: f32, t: &DuckTuning) -> bool {
    let l = &mut self.learned;
    let adopt = l.loud_s < t.loud_settled_s;
    if adopt {
      (l.loud_db, l.loud_s, l.lock_db) = (Some(db), l.loud_s.max(t.loud_settled_s), None);
    }
    adopt
  }

  pub(super) fn seed_quiet(&mut self, db: f32, t: &DuckTuning) -> bool {
    let l = &mut self.learned;
    let adopt = l.quiet_s < t.quiet_settled_s;
    if adopt {
      (l.quiet_db, l.quiet_s, l.lock_db) = (Some(db), l.quiet_s.max(t.quiet_settled_s), None);
    }
    adopt
  }

  /// The loud and quiet levels for the per-game store: only settled ones, a thin estimate must
  /// never reach it.
  pub(super) fn settled_levels(&self, t: &DuckTuning) -> (Option<f32>, Option<f32>) {
    let l = &self.learned;
    (l.loud_db.filter(|_| l.loud_s >= t.loud_settled_s), l.quiet_db.filter(|_| l.quiet_s >= t.quiet_settled_s))
  }

  pub(super) fn status(&self, t: &DuckTuning) -> DuckStatus {
    let l = &self.learned;
    DuckStatus {
      talking: self.talking,
      latched: self.latched && self.talk_s > 0.0, // this spurt ducks, not a latch kept between spurts
      game_db: db(self.game_ms),
      chat_db: self.chat_db,
      spurt_db: (self.talk_s > 0.0).then_some(self.spurt_db),
      spurt_talk_s: self.talk_s,
      need_db: self.need_db,
      depth_db: self.depth_db,
      target_db: self.target_db,
      lock_db: l.lock_db,
      loud_db: l.loud_db,
      quiet_db: l.quiet_db,
      loud_s: l.loud_s,
      quiet_s: l.quiet_s,
      loud_settled: l.loud_s >= t.loud_settled_s,
      quiet_settled: l.quiet_s >= t.quiet_settled_s,
      loud_seeded: false,
      quiet_seeded: false,
    }
  }
}

// ---------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------

impl Learned {
  /// The loud level is an upper envelope of the game level: above it, it rises with
  /// `loud_rise_s`; otherwise it falls at a fixed rate, never below the game.
  fn learn_loud(&mut self, game_db: f32, dt: f32, t: &DuckTuning) {
    self.loud_db = Some(match self.loud_db {
      None => game_db,
      Some(loud) if game_db > loud => smooth(loud, game_db, dt, t.loud_rise_s, t.loud_rise_s),
      Some(loud) => (loud - t.loud_fall_db_per_min * dt / 60.0).max(game_db),
    });
  }

  /// One block of `dt` of a spurt with enough talk, at the spurt's level. Until settled a running
  /// mean, so the first talkers average and an early quiet spurt cannot pin it; then the lower
  /// envelope with the outlier guard.
  fn learn_quiet(&mut self, level: f32, dt: f32, t: &DuckTuning) {
    let settled = self.quiet_s >= t.quiet_settled_s;
    self.quiet_s += dt;
    self.quiet_db = Some(match self.quiet_db {
      None => level,
      Some(quiet) if !settled => quiet + (level - quiet) * dt / self.quiet_s,
      Some(quiet) => {
        let level = level.max(quiet - t.quiet_outlier_db);
        if level < quiet {
          quiet + (level - quiet) * (1.0 - (-dt / t.quiet_fall_s).exp())
        } else {
          (quiet + t.quiet_rise_db_per_min * dt / 60.0).min(level)
        }
      }
    });
  }

  /// Follows loud - `quiet_db` until both levels are settled, then moves only by the excess over
  /// the backlash (`hysteresis_up_db` / `hysteresis_down_db`).
  fn update_lock(&mut self, quiet_db: Option<f32>, t: &DuckTuning) {
    let (Some(loud), Some(quiet)) = (self.loud_db, quiet_db) else { return };
    let diff = loud - quiet;
    let settled = self.loud_s >= t.loud_settled_s && self.quiet_s >= t.quiet_settled_s;
    self.lock_db = Some(match self.lock_db {
      Some(lock) if settled => lock.max(diff - t.hysteresis_up_db).min(diff + t.hysteresis_down_db),
      _ => diff,
    });
  }
}

/// One-pole smoothing with separate rise and fall time constants.
fn smooth(level: f32, input: f32, dt: f32, rise_s: f32, fall_s: f32) -> f32 {
  let tau = if input > level { rise_s } else { fall_s };
  level + (input - level) * (1.0 - (-dt / tau).exp())
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests;
