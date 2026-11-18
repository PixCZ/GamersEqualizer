use super::*;

/// Strength 500.
const DEFAULT: DuckSettings = DuckSettings { enabled: true, strength: 0.5, game_gain: 1.0, chat_gain: 1.0 };
const T: DuckTuning = DuckTuning::DEFAULT;

// --- Whether a spurt ducks: who is talking, over how loud a game ---

#[test]
fn strength_sets_how_soon_and_how_deep() {
  let depth_at = |strength: f32, game_db: f32, voice_db: f32| {
    let settings = DuckSettings { strength, ..DEFAULT };
    let mut gain = DuckGain::default();
    drive_at(&mut gain, &settings, 30.0, game_db, voice_db, |_| false);
    peak(&drive_at(&mut gain, &settings, 3.0, game_db, voice_db, |i| i % 70 < 50))
  };
  // The game 3 dB over the voice: strength 0 ducks a little, every 50 % about 6 dB more.
  let (none, half, full) = (depth_at(0.0, -22.0, -25.0), depth_at(0.5, -22.0, -25.0), depth_at(1.0, -22.0, -25.0));
  assert!(none > 0.0 && none < 4.0, "{none}");
  assert!((half - none - 6.0).abs() < 0.3 && (full - half - 6.0).abs() < 0.3, "{none} {half} {full}");
  // The game 8 dB under the voice: only full strength ducks.
  assert_eq!(depth_at(0.5, -32.0, -24.0), 0.0);
  assert!(depth_at(1.0, -32.0, -24.0) > 2.0);
  assert_eq!(depth_at(1.0, -5.0, -40.0), T.max_depth_db, "the bound");
}

#[test]
fn a_loud_talker_over_the_game_does_not_duck_a_quiet_one_does_by_the_margin() {
  let mut gain = DuckGain::default();
  settle(&mut gain); // learned with the quiet talker at -30 dB
  assert_eq!(sentence_at(&mut gain, -22.0, -12.0), 0.0, "loud talker");
  let quiet = sentence_at(&mut gain, -22.0, -30.0);
  // The game lowered by the depth sits the margin under the quiet talker.
  assert!(((-22.0 - quiet) - (-30.0 - DEFAULT.strength * T.max_margin_db)).abs() < 0.5, "{quiet}");
}

#[test]
fn a_loud_talkers_soft_onset_does_not_latch() {
  let mut gain = DuckGain::default();
  fight_at(&mut gain, -22.0);
  drive(&mut gain, 1.0, -22.0, |_| false);
  let depths = drive_levels(&mut gain, 2.0, -22.0, |i| Some(if i < 5 { -35.0 } else { -10.0 }));
  assert_eq!(peak(&depths), 0.0);
}

#[test]
fn a_quiet_talker_answering_a_loud_one_ducks_within_a_second() {
  let mut gain = DuckGain::default();
  settle(&mut gain);
  drive(&mut gain, 1.0, -22.0, |_| false);
  let loud = drive_voice(&mut gain, 4.0, -22.0, -12.0, |_| true);
  drive(&mut gain, 0.3, -22.0, |_| false); // within the hold: one spurt
  let quiet = drive_voice(&mut gain, 3.0, -22.0, -30.0, |_| true);
  assert_eq!(peak(&loud), 0.0, "the loud talker is above the game");
  assert_eq!(quiet[100], gain.target_db.unwrap(), "at the locked depth one second in");
  assert!((gain.learned.quiet_db.unwrap() + 30.0).abs() < 1.0, "{:?}", gain.learned.quiet_db);
}

// --- The quiet talker level ---

#[test]
fn loud_and_quiet_talkers_alternating_in_a_fight_get_one_depth() {
  let mut gain = DuckGain::default();
  for _ in 0..12 {
    sentence_at(&mut gain, -18.0, -30.0);
  }
  let peaks: Vec<f32> =
    (0..10).flat_map(|_| [sentence_at(&mut gain, -18.0, -22.0), sentence_at(&mut gain, -18.0, -30.0)]).collect();
  assert!(peaks[0] > 10.0 && peaks.iter().all(|p| *p == peaks[0]), "{peaks:?}");
}

#[test]
fn the_first_talkers_average_before_the_quiet_level_settles() {
  let mut gain = DuckGain::default();
  sentence_at(&mut gain, -22.0, -40.0); // an early quiet spurt must not pin the level
  for _ in 0..10 {
    sentence_at(&mut gain, -22.0, -30.0);
  }
  let quiet = gain.learned.quiet_db.unwrap();
  assert!(quiet > -33.0 && quiet < -30.0, "{quiet}");
}

#[test]
fn one_very_quiet_spurt_moves_the_depth_a_little_several_move_it_further() {
  let mut gain = DuckGain::default();
  for _ in 0..20 {
    sentence_at(&mut gain, -22.0, -30.0);
  }
  let settled = sentence_at(&mut gain, -22.0, -30.0);
  sentence_at(&mut gain, -22.0, -45.0); // a whisper or a far-away mic: counts 6 dB under at most
  let after_one = sentence_at(&mut gain, -22.0, -30.0) - settled;
  assert!(after_one > 0.0 && after_one <= 2.5, "{after_one}");
  for _ in 0..10 {
    sentence_at(&mut gain, -22.0, -45.0);
  }
  assert!(sentence_at(&mut gain, -22.0, -30.0) > settled + after_one + 3.0);
}

#[test]
fn the_quiet_talker_leaving_lets_the_level_rise_only_slowly() {
  let mut gain = DuckGain::default();
  for _ in 0..20 {
    sentence_at(&mut gain, -22.0, -30.0);
  }
  let (before, before_s) = (gain.learned.quiet_db.unwrap(), gain.learned.quiet_s);
  for _ in 0..50 {
    sentence_at(&mut gain, -22.0, -15.0); // only a loud talker now
  }
  let (rise, learned_s) = (gain.learned.quiet_db.unwrap() - before, gain.learned.quiet_s - before_s);
  assert!((rise - T.quiet_rise_db_per_min * learned_s / 60.0).abs() < 0.05, "{rise} after {learned_s} s");
}

#[test]
fn a_short_ok_teaches_nothing() {
  let mut gain = DuckGain::default();
  for _ in 0..5 {
    sentence_at(&mut gain, -22.0, -30.0);
  }
  let before = (gain.learned.quiet_db, gain.learned.quiet_s);
  drive_voice(&mut gain, 0.3, -22.0, -45.0, |_| true);
  drive(&mut gain, 1.0, -22.0, |_| false);
  assert_eq!((gain.learned.quiet_db, gain.learned.quiet_s), before);
}

#[test]
fn a_loud_and_a_quiet_talker_from_a_fresh_start_settle_on_the_quiet_one() {
  // A steady fight, a talker at -28 dB and one at -34 dB taking turns: 30 sentences each.
  let mut gain = DuckGain::default();
  let mut quiet_peaks = Vec::new();
  let mut quiet_levels = Vec::new();
  for _ in 0..30 {
    sentence_at(&mut gain, -22.0, -28.0);
    quiet_peaks.push(sentence_at(&mut gain, -22.0, -34.0));
    quiet_levels.push(gain.learned.quiet_db.unwrap());
  }
  // Initial learning with the default tuning: the quiet talker's first 10 sentences (about 100 s
  // of this conversation). Fails with a slow envelope or a wide upward backlash: the depth then
  // stays short of what the next run, seeded, starts at.
  const LEARNED: usize = 10;
  assert!((quiet_levels[LEARNED] + 34.0).abs() < 1.5, "{quiet_levels:?}");
  let settled = &quiet_peaks[LEARNED..];
  let (low, high) = settled.iter().fold((f32::MAX, 0.0f32), |(l, h), p| (l.min(*p), h.max(*p)));
  assert!(high - low <= T.hysteresis_up_db + 1e-4, "{settled:?}");
  // A second run, seeded with what this one stores, from its first sentence.
  let store = |db: f32| (db * 10.0).round() / 10.0;
  let mut next_run = DuckGain::default();
  next_run.seed_loud(store(gain.learned.loud_db.unwrap()), &T);
  next_run.seed_quiet(store(gain.learned.quiet_db.unwrap()), &T);
  let first = fight_at(&mut next_run, -22.0)[0];
  assert!((first - high).abs() < 1.0, "{first} vs {high}");
}

// --- The loud game level ---

#[test]
fn every_fight_of_a_travel_and_fight_session_gets_the_same_depth() {
  // A minute of travel only 10 dB under the fights, then two minutes of fighting with a sentence
  // every 30 s, the first one second in.
  let mut gain = DuckGain::default();
  let mut peaks = Vec::new();
  for _ in 0..4 {
    drive(&mut gain, 60.0, -32.0, |_| false);
    peaks.extend(fight(&mut gain));
  }
  // From the second fight on (the first one learns).
  let (low, high) = peaks[4..].iter().fold((f32::MAX, 0.0f32), |(l, h), p| (l.min(*p), h.max(*p)));
  assert!(low > 10.0 && high - low < 1.0, "{peaks:?}");
}

#[test]
fn a_long_calm_opening_does_not_slow_the_first_fight() {
  // Four minutes of menus and travel before anyone talks, a short first fight, then more.
  let mut gain = DuckGain::default();
  drive(&mut gain, 240.0, -32.0, |_| false);
  assert_eq!(gain.learned.loud_s, 0.0, "no fight time without a voice");
  let mut peaks = Vec::new();
  for seconds in [30.0, 120.0, 120.0, 120.0] {
    drive(&mut gain, 1.0, -22.0, |_| false);
    peaks.push(peak(&drive(&mut gain, 3.0, -22.0, |i| i % 70 < 50)));
    drive(&mut gain, seconds - 4.0, -22.0, |_| false);
    drive(&mut gain, 60.0, -32.0, |_| false);
  }
  // The quiet talker settles after 10 s of learned talk (~1.2 s per sentence, during the third
  // fight): exact from the fourth, the second within a tenth of a dB.
  assert!(peaks[3..].iter().all(|p| *p == peaks[3]) && (peaks[1] - peaks[3]).abs() < 0.1, "{peaks:?}");
}

#[test]
fn ten_minutes_of_calm_cost_five_db() {
  let mut gain = DuckGain::default();
  drive(&mut gain, 30.0, -22.0, |_| false);
  let learned = gain.learned.loud_db.unwrap();
  drive(&mut gain, 600.0, -60.0, |_| false);
  let lost = learned - gain.learned.loud_db.unwrap();
  assert!((lost - 5.0).abs() < 0.1, "{lost} dB");
}

#[test]
fn an_explosion_moves_the_loud_level_less_and_less() {
  let explosion = |gain: &mut DuckGain| {
    let before = gain.learned.loud_db.unwrap();
    drive(gain, 2.0, -10.0, |_| false);
    drive(gain, 10.0, -22.0, |_| false);
    gain.learned.loud_db.unwrap() - before
  };
  // Fight time counts once a voice is known: a sentence at the start of each.
  let mut early = DuckGain::default();
  drive(&mut early, 10.0, -22.0, |i| i < 300);
  let mut late = DuckGain::default();
  drive(&mut late, 300.0, -22.0, |i| i < 300);
  let (early, late) = (explosion(&mut early), explosion(&mut late));
  assert!(early > 3.0 && late < 2.5, "early {early} dB, late {late} dB");
}

// --- The lock: one depth for the run ---

#[test]
fn before_settling_the_lock_follows_the_estimates() {
  let mut gain = DuckGain::default();
  drive(&mut gain, 5.0, -22.0, |_| false);
  for _ in 0..2 {
    drive(&mut gain, 1.0, -22.0, |_| true);
    // No spurt has ended: the running spurt level stands in for the quiet talker.
    let spurt = gain.spurt_db;
    assert_eq!(gain.learned.lock_db, Some(gain.learned.loud_db.unwrap() - spurt));
  }
}

#[test]
fn the_lock_moves_by_the_excess_over_each_side_of_the_hysteresis() {
  let settled =
    Learned { loud_db: Some(-20.0), loud_s: 100.0, quiet_db: Some(-30.0), quiet_s: 100.0, lock_db: Some(10.0) };
  let lock_after = |shift: f32| {
    let mut learned = settled;
    learned.update_lock(Some(-30.0 - shift), &T);
    learned.lock_db.unwrap() - 10.0
  };
  let expected = [3.0 - T.hysteresis_up_db, 0.0, -(6.0 - T.hysteresis_down_db)];
  assert_eq!([lock_after(3.0), lock_after(-3.0), lock_after(-6.0)], expected);
}

#[test]
fn a_settled_depth_ignores_fights_that_wander_within_the_hysteresis() {
  let mut gain = DuckGain::default();
  settle(&mut gain); // locked on a -22 dB fight
  let mut peaks = Vec::new();
  // Quieter fights and back to the learned loudness, with travel between. The backlash is narrow
  // upwards (`hysteresis_up_db`): a fight louder than at the lock would move the depth up.
  for offset in [0.0, -2.0, -1.0, -3.0, 0.0, -2.0] {
    drive(&mut gain, 60.0, -32.0, |_| false);
    peaks.extend(fight_at(&mut gain, -22.0 + offset));
  }
  assert!(peaks.iter().all(|p| *p == peaks[0]), "{peaks:?}");
}

#[test]
fn a_sustained_louder_game_moves_the_depth_by_the_excess_only() {
  let mut gain = DuckGain::default();
  settle(&mut gain);
  let before = *fight_at(&mut gain, -22.0).last().unwrap();
  let mut lock = gain.learned.lock_db.unwrap();
  for _ in 0..12_000 {
    drive(&mut gain, 0.01, -17.0, |_| false);
    let now = gain.learned.lock_db.unwrap();
    assert!((now - lock).abs() < 0.01, "step {lock} -> {now}");
    lock = now;
  }
  let after = *fight_at(&mut gain, -17.0).last().unwrap();
  assert!((after - before - (5.0 - T.hysteresis_up_db)).abs() < 0.3, "{before} -> {after}");
}

#[test]
fn ten_minutes_of_calm_move_the_depth_by_one_db_only() {
  let mut gain = DuckGain::default();
  settle(&mut gain);
  let before = *fight_at(&mut gain, -22.0).last().unwrap();
  drive(&mut gain, 600.0, -60.0, |_| false);
  let after = fight_at(&mut gain, -22.0)[0];
  assert!((before - after - (5.0 - T.hysteresis_down_db)).abs() < 0.3, "{before} -> {after}");
}

#[test]
fn strength_and_volume_changes_after_settling_apply_in_full() {
  let mut gain = DuckGain::default();
  settle(&mut gain);
  let before = *fight_at(&mut gain, -22.0).last().unwrap();
  let sentence = |gain: &mut DuckGain, settings: &DuckSettings| {
    drive_at(gain, settings, 5.0, -22.0, -30.0, |_| false);
    peak(&drive_at(gain, settings, 3.0, -22.0, -30.0, |i| i % 70 < 50))
  };
  let stronger = sentence(&mut gain, &DuckSettings { strength: 1.0, ..DEFAULT });
  let quieter_game = sentence(&mut gain, &DuckSettings { game_gain: 0.5, ..DEFAULT });
  assert!((stronger - before - 6.0).abs() < 0.01, "{before} -> {stronger}");
  assert!((before - quieter_game - 6.02).abs() < 0.01, "{before} -> {quieter_game}");
}

// --- Seeding from an earlier run of the game ---

#[test]
fn a_seeded_stream_ducks_its_first_sentence_like_the_run_that_learned() {
  let mut learner = DuckGain::default();
  let mut peaks = Vec::new();
  for _ in 0..2 {
    drive(&mut learner, 60.0, -32.0, |_| false);
    peaks.extend(fight(&mut learner));
  }
  let saved = |db: f32| (db * 10.0).round() / 10.0; // as the store keeps it
  let (loud, voice) = (saved(learner.learned.loud_db.unwrap()), saved(learner.learned.quiet_db.unwrap()));
  let mut next_run = DuckGain::default();
  drive(&mut next_run, 3.0, -32.0, |_| false); // before the service pairs the stream
  next_run.seed_loud(loud, &T);
  next_run.seed_quiet(voice, &T);
  drive(&mut next_run, 5.0, -32.0, |_| false);
  let first = fight(&mut next_run)[0];
  assert!((first - peaks.last().unwrap()).abs() < 1.0, "{first} vs {peaks:?}");
}

#[test]
fn a_seeded_stream_is_locked_from_its_first_sentence() {
  let mut gain = DuckGain::default();
  gain.seed_loud(-22.0, &T);
  gain.seed_quiet(-30.0, &T);
  // A fight 2 dB quieter than the stored one, inside the backlash: the lock holds the stored
  // difference.
  let first = fight_at(&mut gain, -24.0)[0];
  assert!((first - 14.0).abs() < 1e-4, "{first}");
}

#[test]
fn a_saved_loud_level_too_low_rises_and_too_high_only_decays() {
  let mut low = DuckGain::default();
  low.seed_loud(-32.0, &T);
  drive(&mut low, 120.0, -22.0, |_| false);
  assert!((low.learned.loud_db.unwrap() + 22.0).abs() < 0.5, "{:?}", low.learned.loud_db);
  let mut high = DuckGain::default();
  high.seed_loud(-12.0, &T);
  drive(&mut high, 120.0, -22.0, |_| false);
  assert!((high.learned.loud_db.unwrap() + 13.0).abs() < 0.05, "{:?}", high.learned.loud_db);
}

#[test]
fn a_seeded_quiet_level_too_loud_comes_down_and_too_quiet_rises_only_slowly() {
  let spurts = |gain: &mut DuckGain| (0..16).map(|_| sentence_at(gain, -22.0, -30.0)).last();
  let mut gain = DuckGain::default();
  gain.seed_quiet(-20.0, &T);
  spurts(&mut gain);
  assert!((gain.learned.quiet_db.unwrap() + 30.0).abs() < 1.0, "{:?}", gain.learned.quiet_db);
  // Too quiet (a quiet talker of the last run): rises only at the slow rate, by talk learned.
  let mut gain = DuckGain::default();
  gain.seed_quiet(-40.0, &T);
  spurts(&mut gain);
  let (rise, learned_s) = (gain.learned.quiet_db.unwrap() + 40.0, gain.learned.quiet_s - T.quiet_warm_s);
  assert!((rise - T.quiet_rise_db_per_min * learned_s / 60.0).abs() < 0.02, "{rise} after {learned_s} s");
}

#[test]
fn a_saved_value_after_the_stream_settled_is_ignored() {
  let mut gain = DuckGain::default();
  drive(&mut gain, 12.0, -22.0, |i| i < 1150);
  drive(&mut gain, 25.0, -22.0, |_| false);
  let settled = gain.learned;
  assert!(settled.loud_s >= T.loud_settled_s && settled.quiet_s >= T.quiet_settled_s);
  gain.seed_loud(-5.0, &T);
  gain.seed_quiet(-50.0, &T);
  assert_eq!(gain.learned, settled);
}

/// The gain computer alone for `seconds` of 10 ms blocks at a game band level (dB); the chat
/// says -30 dB in the blocks `talk` picks. Returns the depth after each block.
fn drive(gain: &mut DuckGain, seconds: f32, game_db: f32, talk: impl Fn(usize) -> bool) -> Vec<f32> {
  drive_voice(gain, seconds, game_db, -30.0, talk)
}

fn drive_voice(
  gain: &mut DuckGain,
  seconds: f32,
  game_db: f32,
  voice_db: f32,
  talk: impl Fn(usize) -> bool,
) -> Vec<f32> {
  drive_at(gain, &DEFAULT, seconds, game_db, voice_db, talk)
}

fn drive_at(
  gain: &mut DuckGain,
  settings: &DuckSettings,
  seconds: f32,
  game_db: f32,
  voice_db: f32,
  talk: impl Fn(usize) -> bool,
) -> Vec<f32> {
  drive_levels_at(gain, settings, seconds, game_db, |i| talk(i).then_some(voice_db))
}

/// The voice level per block (None = silent).
fn drive_levels(gain: &mut DuckGain, seconds: f32, game_db: f32, voice: impl Fn(usize) -> Option<f32>) -> Vec<f32> {
  drive_levels_at(gain, &DEFAULT, seconds, game_db, voice)
}

fn drive_levels_at(
  gain: &mut DuckGain,
  settings: &DuckSettings,
  seconds: f32,
  game_db: f32,
  voice: impl Fn(usize) -> Option<f32>,
) -> Vec<f32> {
  let game_ms = 10f32.powf(game_db / 10.0);
  (0..(seconds * 100.0).round() as usize)
    .map(|i| gain.update(0.01, voice(i).map_or(0.0, |db| 10f32.powf(db / 10.0)), game_ms, settings, &T))
    .collect()
}

/// The quiet talker (-30 dB) learned and settled over a -22 dB fight: 12 sentences.
fn settle(gain: &mut DuckGain) {
  for _ in 0..12 {
    sentence_at(gain, -22.0, -30.0);
  }
}

/// One second of game, then a 3 s sentence (one spurt, 2.2 s of talk) at `voice_db`, then 1 s
/// so it ends: the sentence's peak depth.
fn sentence_at(gain: &mut DuckGain, game_db: f32, voice_db: f32) -> f32 {
  drive(gain, 1.0, game_db, |_| false);
  let depth = peak(&drive_voice(gain, 3.0, game_db, voice_db, |i| i % 70 < 50));
  drive(gain, 1.0, game_db, |_| false);
  depth
}

/// Two minutes of fighting at -22 dB with a sentence every 30 s, the first one second in; the
/// peak depth of each sentence.
fn fight(gain: &mut DuckGain) -> Vec<f32> {
  fight_at(gain, -22.0)
}

fn fight_at(gain: &mut DuckGain, game_db: f32) -> Vec<f32> {
  (0..4)
    .map(|_| {
      drive(gain, 1.0, game_db, |_| false);
      let sentence = peak(&drive(gain, 3.0, game_db, |i| i % 70 < 50));
      drive(gain, 26.0, game_db, |_| false);
      sentence
    })
    .collect()
}

fn peak(depths: &[f32]) -> f32 {
  depths.iter().fold(0.0f32, |m, v| m.max(*v))
}
