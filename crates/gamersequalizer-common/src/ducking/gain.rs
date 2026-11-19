//! The gain computer of one game stream: from the game's and the chat's speech-band levels, per
//! block, how deep the game is lowered. No filters, no shared memory.
//!
//! Two levels only: nobody talks, nothing is lowered; someone talks, the game is lowered by the
//! lowering level. That level starts at 0 with every run and is learned from the fights the talk
//! has to get through: a talk spurt the game masks raises it to what that spurt needs, never more,
//! and it comes down only by `LEVEL_DECAY_DB_PER_S`, too slowly to hear. It starts over at 0 after
//! a long silence of the game (a lobby, a menu), a stream gap, the feature turned off, any volume
//! change and a new strength.
//!
//! All levels are as the APO sees them. A per-app session volume (and so the ChatMix balance) is
//! applied before the APO (DEVLOG Milestone 19), so it is already in them; only the volume of a
//! channel on its own output comes after the APO and is added (`game_gain` / `chat_gain`), with
//! the margin, only where a decision is made.

use super::{DuckSettings, DuckTuning, db};

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

/// The game counts as not heard below this (current band level with the channel volume, dB).
/// shortcut: fixed, a tunable if a game's menus sit above it; fights in a test recording were
/// about -34 dB, calm play -52 .. -64 dB, so calm play keeps the level.
const SILENT_GAME_DB: f32 = -65.0;
/// That long without the game heard and the lowering level starts over (a lobby, a menu, a pause).
/// shortcut: fixed, a tunable if it lets go in a long quiet stretch of play.
pub(super) const SILENT_RESET_S: f32 = 60.0;
/// How fast the lowering level comes down while the game is heard: 1 dB in 10 minutes, far under
/// what an ear notices (about 1 dB at once), so the level follows a game that turned calmer for
/// good without anyone hearing it move. A fight that needs more raises it again by that little.
/// shortcut: fixed, a tunable if a game's fights are much further apart than 10 minutes.
const LEVEL_DECAY_DB_PER_S: f32 = 1.0 / 600.0;

// ---------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------

/// What a game stream's ducker saw and decided in its last block (for the telemetry and
/// `duck_sim`). Levels are raw dB.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DuckStatus {
  pub talking: bool,
  /// This spurt is lowered (someone talks and the lowering level is above 0).
  pub lowering: bool,
  pub game_db: f32,
  /// The chat streams' summed speech level this block.
  pub chat_db: f32,
  pub spurt_db: Option<f32>,
  pub spurt_talk_s: f32,
  pub need_db: Option<f32>,
  pub depth_db: f32,
  /// The lowering level: the depth while someone talks.
  pub level_db: f32,
  /// How many spurts raised the lowering level since it last started over.
  pub raises: u32,
}

/// The decision state of one stream. All of it depends on past audio and starts over on a restart
/// (`restart`): nothing outlives the stream.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct DuckGain {
  depth_db: f32,
  /// The lowering level, dB (>= 0).
  level_db: f32,
  /// Spurts that raised it, and whether this spurt did (each counts once).
  raises: u32,
  spurt_raised: bool,
  /// Current game level, mean square.
  game_ms: f32,
  /// How long the game has not been heard, seconds.
  silent_s: f32,
  /// Hold left after the last talking block, seconds: pauses shorter than this stay in the spurt.
  hold_s: f32,
  /// Talk time of this spurt and its level (dB, a leaky mean over `spurt_level_s`).
  talk_s: f32,
  spurt_db: f32,
  /// The channel volumes and the strength of the last block: a change starts the lowering level
  /// over.
  settings: Option<(f32, f32, f32)>,
  /// The last block's inputs and decision, for `DuckStatus`.
  talking: bool,
  chat_db: f32,
  need_db: Option<f32>,
}

// ---------------------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------------------

impl DuckGain {
  /// One block of `dt` seconds: `chat_ms` / `game_ms` are the summed raw speech-band levels of the
  /// chat and the game streams, the game's without this ducker's own lowering. Returns the depth
  /// (dB) the game is lowered by.
  pub(super) fn update(&mut self, dt: f32, chat_ms: f32, game_ms: f32, s: &DuckSettings, t: &DuckTuning) -> f32 {
    // The game level runs while nobody talks too, so it is known when speech starts.
    self.game_ms = smooth(self.game_ms, game_ms, dt, t.game_rise_s, t.game_fall_s);
    let game_db = db(self.game_ms);
    let seen = (s.game_gain, s.chat_gain, s.strength);
    if self.settings.is_some_and(|was| was != seen) || !s.enabled {
      self.relearn();
    }
    self.settings = Some(seen);
    // A lower bound from the tuning applies at once (through the release).
    self.level_db = (self.level_db - LEVEL_DECAY_DB_PER_S * dt).max(0.0);
    self.level_db = self.level_db.min(t.max_depth_db);
    let heard_db = game_db + db(s.game_gain * s.game_gain);
    self.silent_s = if heard_db < SILENT_GAME_DB { self.silent_s + dt } else { 0.0 };
    if self.silent_s >= SILENT_RESET_S {
      self.relearn();
    }

    let talking = s.enabled && s.chat_gain > 0.0 && db(chat_ms) > t.talk_threshold_db;
    let talked_s = self.talk_s;
    if talking {
      self.talk_s += dt;
      // Leaky, so a quiet talker answering a loud one within the hold is heard at their own level
      // within about `spurt_level_s`.
      let weight = if self.talk_s < t.spurt_level_s { dt / self.talk_s } else { 1.0 - (-dt / t.spurt_level_s).exp() };
      self.spurt_db += (db(chat_ms) - self.spurt_db) * weight;
      self.hold_s = t.hold_s;
    } else {
      self.hold_s = if s.enabled { (self.hold_s - dt).max(0.0) } else { 0.0 };
      if self.hold_s <= 0.0 {
        (self.talk_s, self.spurt_db, self.spurt_raised) = (0.0, 0.0, false);
      }
    }
    let spurt_db = (self.talk_s > 0.0).then_some(self.spurt_db);

    // What the listener hears: the volumes after the APO (channel outputs) and the margin.
    let gains_db = db(s.game_gain * s.game_gain) - db(s.chat_gain * s.chat_gain);
    let margin_db = s.strength * t.max_margin_db;
    self.need_db = spurt_db.map(|spurt| game_db - spurt + gains_db + margin_db);
    // Raised only by a spurt that is decided (`spurt_min_s`, so a soft onset cannot) and talking
    // now, to what it needs: the game is lowered as far as the loudest fight so far needed.
    let wanted = self.need_db.filter(|need| talking && self.talk_s >= t.spurt_min_s && *need > t.on_threshold_db);
    let raised = wanted.map(|need| need.min(t.max_depth_db)).filter(|need| *need > self.level_db);
    if let Some(level) = raised {
      self.raises += u32::from(!self.spurt_raised);
      (self.level_db, self.spurt_raised) = (level, true);
    }
    (self.talking, self.chat_db) = (talking, db(chat_ms));

    let goal_db = if spurt_db.is_none() {
      0.0 // nobody talks: nothing lowered
    } else if talking {
      self.level_db
    } else {
      self.depth_db.min(self.level_db) // a pause between words: held, unless the level started over
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

  /// A stream gap or going idle: everything starts over, the lowering level included.
  pub(super) fn restart(&mut self) {
    *self = DuckGain::default();
  }

  /// A volume before the measurement moved (a session volume, `game_db` / `chat_db` its change):
  /// the current levels move with it, and the lowering level is learned again from 0 at the new
  /// volumes.
  pub(super) fn shift(&mut self, game_db: f32, chat_db: f32) {
    self.game_ms *= 10f32.powf(game_db / 10.0);
    self.spurt_db += chat_db;
    self.relearn();
  }

  pub(super) fn status(&self) -> DuckStatus {
    let spurt = self.talk_s > 0.0;
    DuckStatus {
      talking: self.talking,
      lowering: spurt && self.level_db > 0.0,
      game_db: db(self.game_ms),
      chat_db: self.chat_db,
      spurt_db: spurt.then_some(self.spurt_db),
      spurt_talk_s: self.talk_s,
      need_db: self.need_db,
      depth_db: self.depth_db,
      level_db: self.level_db,
      raises: self.raises,
    }
  }
}

// ---------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------

impl DuckGain {
  /// The lowering level starts over at 0; a depth still applied comes back at the release rate.
  fn relearn(&mut self) {
    (self.level_db, self.raises, self.spurt_raised, self.silent_s) = (0.0, 0, false, 0.0);
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
