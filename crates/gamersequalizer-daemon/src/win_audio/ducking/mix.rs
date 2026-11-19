//! Mix mode: a game the APO cannot hear at its stream (`unheard`: with spatial sound on it plays
//! past the per-app effects) is lowered by the service instead, through the Game channel's
//! per-app session volume, which Windows applies before the spatial renderer. The game is
//! measured in the output mix (the speaker reference the output APO writes, the same ring the echo
//! canceller reads) while the chat makes no sound, and held while it does (`MixDucker`); the cut
//! is full band, not only the speech range.
//!
//! Threads: `MixRun::step` runs on the reference thread (`apo_link`, which holds the APO's mapping)
//! at every reference tick, 40 ms while something moves; the depth goes to the service loop, the one
//! writer of the session volumes (`ChatMixController::apply_session_mix`), through `DEPTH_X100`.
//! The service loop tells it whether it owns the session volumes (`SESSION_MODE`) and the Game /
//! Chat session volumes it sets (`SESSION_GAINS`): the levels are measured after them, so what was
//! learned moves with them, and the per-game store keeps the levels as at 100 %.

use gamersequalizer_common::ducking::dbg::{
  self, BAND_DB, DEPTH_DB, FLAG_LATCHED, FLAG_LOUD_SEEDED, FLAG_LOUD_SETTLED, FLAG_QUIET_SEEDED, FLAG_QUIET_SETTLED,
  FLAG_TALKING, FLAGS, GAME_DB, LOCKED_DIFF_DB, LOUD_EVIDENCE_S, NEED_DB, QUIET_EVIDENCE_S, SPURT_DB, TARGET_DEPTH_DB,
  VOICE_SUM_DB,
};
use gamersequalizer_common::ducking::{DuckSettings, DuckStatus, DuckTuning, MixDucker, chat_levels, chat_sounding};
use gamersequalizer_common::shm::{REF_SAMPLE_RATE, ShmLayout, StereoFrame};
use gamersequalizer_common::streams::DUCK_DBG_SLOTS;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use super::level_store::{LevelStore, valid};
use super::view::StreamView;
use crate::win_audio::stream_router;

/// Most frames read per step (0.5 s): after a stall only the newest count, older ones are stale.
const MAX_FRAMES: u32 = 24_000;
/// The speaker reference counts as missing only after no frames for this long since it was asked
/// for (an idle output writes nothing for a moment); the window uses the same span for its warning.
pub const REFERENCE_MISSING: Duration = Duration::from_secs(5);
/// A reference sample beyond this is garbage, not audio.
const SAMPLE_MAX: f32 = 8.0;
/// Owner value of the mix-mode line in the log (never a slot's: those carry an audiodg pid).
const MIX_OWNER: u64 = u64::MAX;
/// A session volume at or below this is muted: it says nothing about the level once unmuted.
const MUTED_GAIN: f32 = 0.001;

/// Service loop -> reference thread: the mixer owns the per-app session volumes.
static SESSION_MODE: AtomicBool = AtomicBool::new(false);
/// Service loop -> reference thread: the Game and Chat session volumes (linear, f32 bits) without
/// this module's own lowering.
static SESSION_GAINS: [AtomicU32; 2] = [AtomicU32::new(1.0f32.to_bits()), AtomicU32::new(1.0f32.to_bits())];
/// Reference thread -> service loop: the depth (dB x 100) to fold into the Game sessions' volume.
static DEPTH_X100: AtomicU32 = AtomicU32::new(0);
/// Reference thread -> both loops: a reduction moves or holds, tick fast (`mix_fast`).
static FAST: AtomicBool = AtomicBool::new(false);
/// Reference thread -> service loop: the exe path of the game mix mode lowers (`mix_subject`),
/// only its sessions; None while mix mode neither runs nor brings a reduction back.
static SUBJECT: Mutex<Option<String>> = Mutex::new(None);

/// Who lowers a game while Automatic ChatMix is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
  /// The APO, at the game's stream (the game is heard there).
  Stream,
  /// The service, through the game's session volume, from the output mix.
  Mix,
  /// Neither: the game plays past the per-app effects and the Game / Chat channels have their own
  /// outputs, so the session volumes are not the mixer's.
  ChannelOutputs,
  /// Neither: the game plays past the per-app effects and the output mix does not reach the
  /// service (no output effect on that output).
  NoReference,
}

/// Frames of the reference ring the last read has not seen.
#[derive(Debug, Default)]
pub struct RingCursor {
  next: Option<u32>,
}

/// One game's mix-mode ducker, kept for the service's run: a pause (a menu sound through the
/// game's stream) forgets past audio, not what it learned.
struct GameRun {
  ducker: MixDucker,
  /// The stored levels it was handed (None: not asked yet), once per run.
  seeded: Option<(i32, i32)>,
  paused: bool,
}

/// Mix mode on the reference thread.
pub struct MixRun {
  games: HashMap<String, GameRun>,
  /// The game it is about (the paired game stream's exe, lowercase, and its image path), kept while
  /// it stays the subject.
  key: Option<String>,
  path: Option<String>,
  /// Another game became the subject while a reduction was on: it takes over once the old game's
  /// depth has come back to 0 (the old one gets its volume back at the release rate, not at once).
  pending: Option<(String, String)>,
  /// What `SUBJECT` says now.
  published: Option<String>,
  /// The rule says the game cannot be heard at its stream (set every 100 ms by `Ducking::tick`).
  unheard: bool,
  cursor: RingCursor,
  frames: Vec<f32>,
  last_frames_at: Option<Instant>,
  last_step: Option<Instant>,
  mode: Mode,
  depth_db: f32,
  /// Mix level of the last step and the chat band level used, dB.
  mix_db: f32,
  chat_band_db: f32,
  /// The chat made sound in the last step: the game's level was held, not measured.
  chat_sounding: bool,
  /// The Game and Chat session volumes (dB) the learned levels are at; None before the first look.
  gains_db: Option<(f32, f32)>,
}

// ---------------------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------------------

/// Service loop: whether the mixer owns the per-app session volumes now, and the Game and Chat
/// session volumes it sets (`ChatMixController::get_multipliers`).
/// shortcut: a session the user moved by hand in the Windows mixer is not seen; it is relearned slowly.
pub fn set_session_mode(on: bool, (game, chat): (f32, f32)) {
  SESSION_MODE.store(on, Ordering::Relaxed);
  SESSION_GAINS[0].store(game.to_bits(), Ordering::Relaxed);
  SESSION_GAINS[1].store(chat.to_bits(), Ordering::Relaxed);
}

/// Service loop: the factor to fold into the Game sessions' volume (1 = untouched).
pub fn game_duck_factor() -> f32 {
  let depth_db = DEPTH_X100.load(Ordering::Relaxed) as f32 / 100.0;
  10f32.powf(-depth_db / 20.0).clamp(0.0, 1.0)
}

/// Service loop: the game mix mode lowers (exe path), next to `game_duck_factor`; empty if its
/// lock was poisoned.
pub fn mix_subject() -> Option<String> {
  SUBJECT.lock().map(|s| s.clone()).unwrap_or_default()
}

/// The Game and Chat session volumes `set_session_mode` was given, for `step` / `seed_and_learn`.
pub fn session_gains() -> (f32, f32) {
  let [game, chat] = &SESSION_GAINS;
  (f32::from_bits(game.load(Ordering::Relaxed)), f32::from_bits(chat.load(Ordering::Relaxed)))
}

/// A reduction moves or holds: both loops tick fast so the volume moves in small steps.
pub fn mix_fast() -> bool {
  FAST.load(Ordering::Relaxed)
}

/// Who lowers the game. Mix only for a game the APO cannot hear: never both at once.
pub fn mode(unheard: bool, session_mode: bool, reference_flowing: bool) -> Mode {
  match (unheard, session_mode, reference_flowing) {
    (false, _, _) => Mode::Stream,
    (true, false, _) => Mode::ChannelOutputs,
    (true, true, false) => Mode::NoReference,
    (true, true, true) => Mode::Mix,
  }
}

/// The store key of a game's mix-mode levels: measured from the mix, not at the stream, and kept
/// as at 100 % session volume (`@100`; records under the earlier `[mix]` key are as measured, at
/// whatever volume was set, so they are left unread).
pub fn mix_key(game: &str) -> String {
  format!("{game} [mix@100]")
}

impl RingCursor {
  /// The frames written since the last call into `out` (interleaved stereo, cleared first). The
  /// first call, a writer that restarted (its index went back) and a stall longer than
  /// `MAX_FRAMES` start over from the newest frames. False when it started over.
  pub fn take(&mut self, write_pos: u32, ring: &[StereoFrame], out: &mut Vec<f32>) -> bool {
    out.clear();
    let Some(next) = self.next.replace(write_pos) else { return false };
    let ahead = write_pos.wrapping_sub(next);
    if ahead > u32::MAX / 2 || ring.is_empty() {
      return false;
    }
    let count = ahead.min(MAX_FRAMES);
    let start = write_pos.wrapping_sub(count);
    let clean = |v: f32| if v.is_finite() { v.clamp(-SAMPLE_MAX, SAMPLE_MAX) } else { 0.0 };
    for i in 0..count {
      let index = start.wrapping_add(i) as usize % ring.len();
      let frame = ring.get(index).copied().unwrap_or_default();
      out.extend([clean(frame.left), clean(frame.right)]);
    }
    ahead <= MAX_FRAMES
  }
}

impl MixRun {
  pub fn new() -> Self {
    Self {
      games: HashMap::new(),
      key: None,
      path: None,
      pending: None,
      published: None,
      unheard: false,
      cursor: RingCursor::default(),
      frames: Vec::new(),
      last_frames_at: None,
      last_step: None,
      mode: Mode::Stream,
      depth_db: 0.0,
      mix_db: 0.0,
      chat_band_db: 0.0,
      chat_sounding: false,
      gains_db: None,
    }
  }

  /// Every 100 ms from `Ducking::tick`: whether the game cannot be heard at its stream, and which
  /// (the paired game stream's exe, lowercase). The subject stays while nothing names another.
  pub fn set_unheard(&mut self, unheard: bool, subject: Option<(String, String)>) {
    self.unheard = unheard;
    if let Some((key, path)) = subject.filter(|_| unheard) {
      if self.depth_db > 0.0 && self.key.as_ref().is_some_and(|k| *k != key) {
        self.pending = Some((key, path));
      } else {
        (self.key, self.path, self.pending) = (Some(key), Some(path), None);
      }
    }
  }

  /// The speaker reference is needed (`aec_reference` asks the output APO to write it).
  pub fn wants_reference(&self) -> bool {
    self.unheard && SESSION_MODE.load(Ordering::Relaxed)
  }

  pub fn mode(&self) -> Mode {
    self.mode
  }

  /// Every reference tick: read the mix since the last step and lower the game, or let it come
  /// back at the release rate. `layout`: the APO's mapping while we drive it; `gains`: the Game and
  /// Chat session volumes (`session_gains`).
  pub fn step(&mut self, layout: Option<&ShmLayout>, settings: &DuckSettings, tuning: &DuckTuning, gains: (f32, f32)) {
    self.follow_gains(gains);
    let now = Instant::now();
    // At most 0.5 s: a long pause (the thread was held up) must not jump the depth.
    let dt = self.last_step.map_or(0.0, |t| now.duration_since(t).as_secs_f32()).min(0.5);
    self.last_step = Some(now);
    let wanted = self.wants_reference() && settings.enabled;
    let continuous = match layout.filter(|_| wanted) {
      Some(l) => {
        // Asked for just now: it gets `REFERENCE_MISSING` to arrive.
        self.last_frames_at.get_or_insert(now);
        self.cursor.take(l.header.write_frame_index.load(Ordering::Acquire), &l.buffer, &mut self.frames)
      }
      None => {
        (self.cursor, self.last_frames_at) = (RingCursor::default(), None);
        self.frames.clear();
        false
      }
    };
    if !self.frames.is_empty() {
      self.last_frames_at = Some(now);
    }
    let reference = self.last_frames_at.is_some_and(|t| now.duration_since(t) < REFERENCE_MISSING);
    self.mode = mode(self.unheard && settings.enabled, SESSION_MODE.load(Ordering::Relaxed), reference);
    let game = self.key.as_ref().map(|key| {
      self.games.entry(key.clone()).or_insert_with(|| GameRun {
        ducker: MixDucker::new(REF_SAMPLE_RATE as f32),
        seeded: None,
        paused: true,
      })
    });
    let mut talking = false;
    let switching = self.pending.is_some();
    match (self.mode, game, layout, stream_router::now_hns()) {
      (Mode::Mix, Some(game), Some(l), Some(now_hns)) if !switching => {
        if !continuous {
          // A stall or a writer restart: only the audio continuity starts over, the depth stays.
          game.ducker.resync();
        }
        game.paused = false;
        // One snapshot per `process`, passed as it is: whether the chat made sound over this step
        // (the game's level is measured only when not), and its speech level. The band level is
        // for the log only.
        let sounding = chat_sounding(&l.streams, now_hns, dt + 0.02, tuning.mix_chat_sound_db);
        let (speech_ms, band_ms) = chat_levels(&l.streams, now_hns);
        self.depth_db = game.ducker.process(&self.frames, dt, speech_ms, sounding, settings, tuning).max(0.0);
        let mix_ms = self.frames.iter().map(|v| v * v).sum::<f32>() / self.frames.len().max(1) as f32;
        (self.mix_db, self.chat_band_db, self.chat_sounding) = (db(mix_ms), db(band_ms), sounding);
        talking = game.ducker.status().talking;
      }
      (_, game, _, _) => {
        // Paused (feature off, spatial sound off, a menu sound through the game's stream, the game
        // gone): its ducker forgets past audio once and keeps what it learned; the volume comes
        // back at the release rate, never with a step.
        if let Some(game) = game.filter(|g| !g.paused) {
          game.ducker.reset();
          game.paused = true;
        }
        self.depth_db = (self.depth_db - tuning.mix_release_db_per_s * dt).max(0.0);
      }
    }
    if self.depth_db == 0.0
      && let Some((key, path)) = self.pending.take()
    {
      (self.key, self.path) = (Some(key), Some(path));
    }
    // The subject first, then the depth: the service loop never lowers another game.
    let subject = self.path.clone().filter(|_| self.active());
    if subject != self.published {
      if let Ok(mut s) = SUBJECT.lock() {
        s.clone_from(&subject);
      }
      self.published = subject;
    }
    DEPTH_X100.store((self.depth_db * 100.0).round() as u32, Ordering::Relaxed);
    FAST.store(self.depth_db > 0.0 || talking, Ordering::Relaxed);
  }

  /// When the session volumes moved, every game's levels move with them. Returns the volumes (dB)
  /// the levels are at now.
  fn follow_gains(&mut self, gains: (f32, f32)) -> (f32, f32) {
    let now = gains_db(self.gains_db, gains);
    if let Some((game_was, chat_was)) = self.gains_db
      && now != (game_was, chat_was)
    {
      self.games.values_mut().for_each(|g| g.ducker.shift(now.0 - game_was, now.1 - chat_was));
    }
    self.gains_db = Some(now);
    now
  }

  /// Mix mode runs, or its reduction is still coming back.
  fn active(&self) -> bool {
    self.mode == Mode::Mix || self.depth_db > 0.0
  }

  /// Every 100 ms: hands the stored levels to a new run once, keeps what it settled on. Returns
  /// the levels handed (for the log), once. `gains`: as for `step`.
  pub fn seed_and_learn(&mut self, store: &mut LevelStore, gains: (f32, f32)) -> Option<(i32, i32)> {
    let (game_db, chat_db) = self.follow_gains(gains);
    let key = self.key.as_deref()?;
    let game = self.games.get_mut(key)?;
    let key = mix_key(key);
    let mut handed = None;
    if game.seeded.is_none() {
      let levels = store.get(&key).unwrap_or((0, 0));
      let db = |x10: i32| (x10 != 0).then_some(x10 as f32 / 10.0);
      // Stored as at 100 %: at the volumes now.
      game.ducker.seed(db(levels.0).map(|v| v + game_db), db(levels.1).map(|v| v + chat_db));
      game.seeded = Some(levels);
      handed = (levels != (0, 0)).then_some(levels);
    }
    let (loud, quiet) = game.ducker.learned();
    let x10 = |v: Option<f32>, at: f32| valid(v.map_or(0, |db| ((db - at) * 10.0).round() as i32));
    store.learn(&key, (x10(loud, game_db), x10(quiet, chat_db)));
    handed
  }

  /// The run as a game stream for the log (`minute_summary`, `debug_log`), marked "(mix)".
  pub fn view(&self, name: &str) -> Option<StreamView> {
    let game = self.key.as_ref().and_then(|key| self.games.get(key)).filter(|_| self.active())?;
    let st = game.ducker.status();
    let (loud, quiet) = game.ducker.learned();
    let x10 = |v: Option<f32>| valid(v.map_or(0, |db| (db * 10.0).round() as i32));
    Some(StreamView {
      index: 0,
      owner: MIX_OWNER,
      is_game: true,
      exe: format!("{name} (mix)"),
      path: String::new(),
      sample_rate: REF_SAMPLE_RATE,
      channels: 2,
      fresh: true,
      duck_permille: (1000.0 * (1.0 - 10f32.powf(-self.depth_db / 20.0))).round() as u32,
      speech_ms: 10f32.powf(st.game_db / 10.0),
      dbg: telemetry(&st),
      learned: (x10(loud), x10(quiet)),
      saved: game.seeded.unwrap_or((0, 0)),
      mix: Some((to_x10(self.mix_db), to_x10(self.chat_band_db), self.chat_sounding)),
    })
  }
}

impl Default for MixRun {
  fn default() -> Self {
    Self::new()
  }
}

// ---------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------

/// The telemetry a slot would carry for this status (`StreamDucker`'s publish), so the log reads
/// a mix-mode run with the same code as a stream.
/// The session volumes in dB; a muted or broken one keeps its previous value (0 dB at first).
fn gains_db(previous: Option<(f32, f32)>, (game, chat): (f32, f32)) -> (f32, f32) {
  let to_db = |gain: f32, was: f32| if gain.is_finite() && gain > MUTED_GAIN { 20.0 * gain.log10() } else { was };
  let (game_was, chat_was) = previous.unwrap_or((0.0, 0.0));
  (to_db(game, game_was), to_db(chat, chat_was))
}

fn telemetry(st: &DuckStatus) -> [i32; DUCK_DBG_SLOTS] {
  let mut d = [dbg::NA; DUCK_DBG_SLOTS];
  let mut set = |index: usize, value: i32| {
    if let Some(cell) = d.get_mut(index) {
      *cell = value;
    }
  };
  let level = |v: Option<f32>| v.map_or(dbg::NA, to_x10);
  let flags = [
    (st.talking, FLAG_TALKING),
    (st.latched, FLAG_LATCHED),
    (st.loud_settled, FLAG_LOUD_SETTLED),
    (st.quiet_settled, FLAG_QUIET_SETTLED),
    (st.loud_seeded, FLAG_LOUD_SEEDED),
    (st.quiet_seeded, FLAG_QUIET_SEEDED),
  ]
  .iter()
  .filter(|(on, _)| *on)
  .fold(0, |bits, (_, flag)| bits | flag);
  set(BAND_DB, to_x10(st.game_db));
  set(GAME_DB, to_x10(st.game_db));
  set(VOICE_SUM_DB, to_x10(st.chat_db));
  set(SPURT_DB, level(st.spurt_db));
  set(NEED_DB, level(st.need_db));
  set(DEPTH_DB, to_x10(st.depth_db));
  set(TARGET_DEPTH_DB, level(st.target_db));
  set(LOCKED_DIFF_DB, level(st.lock_db));
  set(FLAGS, flags);
  set(LOUD_EVIDENCE_S, st.loud_s as i32);
  set(QUIET_EVIDENCE_S, st.quiet_s as i32);
  d
}

/// dB x 10 (saturating, NaN reads 0).
fn to_x10(db: f32) -> i32 {
  (db * 10.0).round() as i32
}

/// A mean square in dB, silence floored at -120 dB.
fn db(ms: f32) -> f32 {
  10.0 * ms.max(1e-12).log10()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ring(len: usize) -> Vec<StereoFrame> {
    (0..len).map(|i| StereoFrame { left: i as f32, right: -(i as f32) }).collect()
  }

  #[test]
  fn mix_mode_only_for_a_game_the_apo_cannot_hear() {
    for session in [false, true] {
      for flowing in [false, true] {
        assert_eq!(mode(false, session, flowing), Mode::Stream, "never both at once");
      }
    }
    assert_eq!(mode(true, false, true), Mode::ChannelOutputs);
    assert_eq!(mode(true, true, false), Mode::NoReference);
    assert_eq!(mode(true, true, true), Mode::Mix);
  }

  #[test]
  fn the_cursor_reads_what_arrived_across_wrap_and_writer_restart() {
    let (ring, mut out, mut c) = (ring(8), Vec::new(), RingCursor::default());
    assert!(!c.take(u32::MAX - 2, &ring, &mut out) && out.is_empty(), "the first read starts at the newest");
    // Wrap-around of the index: 6 frames, positions u32::MAX - 2 .. 2 (ring slots by modulo).
    assert!(c.take(3, &ring, &mut out));
    let expected: Vec<f32> = [u32::MAX - 2, u32::MAX - 1, u32::MAX, 0, 1, 2]
      .iter()
      .flat_map(|p| {
        let i = (*p as usize % 8) as f32;
        [i, -i]
      })
      .collect();
    assert_eq!(out, expected);
    assert!(c.take(3, &ring, &mut out) && out.is_empty(), "nothing new");
    // The writer restarted (index back at 0): start over, nothing stale.
    assert!(!c.take(1, &ring, &mut out) && out.is_empty());
    assert!(c.take(2, &ring, &mut out) && out.len() == 2);
    // A stall: only the newest MAX_FRAMES.
    assert!(!c.take(2 + MAX_FRAMES * 3, &ring, &mut out));
    assert_eq!(out.len(), 2 * MAX_FRAMES as usize);
  }

  #[test]
  fn garbage_in_the_ring_reads_as_silence() {
    let ring = vec![StereoFrame { left: f32::NAN, right: 1e9 }; 4];
    let (mut out, mut c) = (Vec::new(), RingCursor::default());
    c.take(0, &ring, &mut out);
    c.take(1, &ring, &mut out);
    assert_eq!(out, [0.0, SAMPLE_MAX]);
  }

  #[test]
  fn mix_levels_have_their_own_store_key() {
    assert_eq!(mix_key("thedivision2.exe"), "thedivision2.exe [mix@100]");
  }

  #[test]
  fn the_subject_stays_and_its_ducker_survives_a_pause() {
    let mut run = MixRun::new();
    run.set_unheard(true, Some(("game.exe".into(), r"\Device\HarddiskVolume3\game.exe".into())));
    let tuning = DuckTuning::default();
    let settings = DuckSettings { enabled: true, strength: 0.5, game_gain: 1.0, chat_gain: 1.0 };
    // No mapping: not mix mode, but the game's run exists and keeps its seed across pauses.
    run.step(None, &settings, &tuning, (1.0, 1.0));
    run.games.get_mut("game.exe").unwrap().seeded = Some((-200, -300));
    run.set_unheard(false, None); // a menu sound through the stream: paused
    run.step(None, &settings, &tuning, (1.0, 1.0));
    run.set_unheard(true, None); // back, and nothing names another game
    run.step(None, &settings, &tuning, (1.0, 1.0));
    assert_eq!(run.key.as_deref(), Some("game.exe"));
    assert_eq!(run.games.len(), 1);
    assert_eq!(run.games["game.exe"].seeded, Some((-200, -300)), "not handed again");
  }

  #[test]
  fn a_session_volume_change_moves_the_levels_and_the_store_keeps_them_as_at_full_volume() {
    let dir = std::env::temp_dir().join(format!("geq_mix_gains_{}", std::process::id()));
    let mut store = LevelStore::load(dir.join("levels.json"));
    store.learn(&mix_key("game.exe"), (-200, -300)); // as at 100 %
    let mut run = MixRun::new();
    run.set_unheard(true, Some(("game.exe".into(), r"\Device\HarddiskVolume3\game.exe".into())));
    let settings = DuckSettings { enabled: true, strength: 0.5, game_gain: 1.0, chat_gain: 1.0 };
    run.step(None, &settings, &DuckTuning::default(), (0.5, 1.0));
    run.seed_and_learn(&mut store, (0.5, 1.0));
    let learned = |run: &MixRun| run.games["game.exe"].ducker.learned();
    let (loud, quiet) = learned(&run);
    assert!((loud.unwrap() + 26.02).abs() < 0.01 && quiet == Some(-30.0), "seeded at -6 dB: {loud:?} {quiet:?}");
    run.seed_and_learn(&mut store, (1.0, 1.0)); // the game turned up to 100 %
    assert!((learned(&run).0.unwrap() + 20.0).abs() < 0.01, "{:?}", learned(&run));
    assert_eq!(store.get(&mix_key("game.exe")), Some((-200, -300)), "stored as at 100 % throughout");
    let _ = std::fs::remove_dir_all(dir);
  }

  #[test]
  fn session_volumes_in_db_keep_the_last_audible_value_through_a_mute() {
    let at_half = gains_db(None, (0.5, 1.0));
    assert!((at_half.0 + 6.02).abs() < 0.01 && at_half.1 == 0.0, "{at_half:?}");
    assert_eq!(gains_db(Some(at_half), (0.0, f32::NAN)), at_half, "muted and garbage keep the last");
  }

  #[test]
  fn the_factor_follows_the_depth() {
    DEPTH_X100.store(0, Ordering::Relaxed);
    assert_eq!(game_duck_factor(), 1.0);
    DEPTH_X100.store(2000, Ordering::Relaxed);
    assert!((game_duck_factor() - 0.1).abs() < 1e-4);
    DEPTH_X100.store(0, Ordering::Relaxed);
  }
}
