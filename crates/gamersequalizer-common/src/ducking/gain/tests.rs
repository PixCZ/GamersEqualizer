use super::*;

/// Strength 500.
const DEFAULT: DuckSettings = DuckSettings { enabled: true, strength: 0.5, game_gain: 1.0, chat_gain: 1.0 };
const T: DuckTuning = DuckTuning::DEFAULT;

// --- Whether a spurt raises the level: who is talking, over how loud a game ---

#[test]
fn strength_sets_how_soon_and_how_deep() {
  let depth_at = |strength: f32, game_db: f32, voice_db: f32| {
    let settings = DuckSettings { strength, ..DEFAULT };
    let mut gain = DuckGain::default();
    drive_at(&mut gain, &settings, 30.0, game_db, voice_db, |_| false);
    peak(&drive_at(&mut gain, &settings, 3.0, game_db, voice_db, |i| i % 70 < 50))
  };
  // The game 3 dB over the voice: strength 0 ducks a little, every 50 % half the margin more (up
  // to the bound).
  let (none, half, full) = (depth_at(0.0, -22.0, -25.0), depth_at(0.5, -22.0, -25.0), depth_at(1.0, -22.0, -25.0));
  let step = T.max_margin_db / 2.0;
  assert!(none > 0.0 && none < 4.0, "{none}");
  assert!((half - none - step).abs() < 0.3, "{none} {half}");
  assert!((full - (half + step).min(T.max_depth_db)).abs() < 0.3, "{half} {full}");
  // The game 8 dB under the voice: only full strength ducks.
  assert_eq!(depth_at(0.5, -32.0, -24.0), 0.0);
  assert!(depth_at(1.0, -32.0, -24.0) > 2.0);
  assert_eq!(depth_at(1.0, -5.0, -40.0), T.max_depth_db, "the bound");
}

#[test]
fn a_loud_talker_over_the_game_does_not_duck_a_quiet_one_does_by_the_margin() {
  let mut gain = DuckGain::default();
  assert_eq!(sentence_at(&mut gain, -22.0, -12.0), 0.0, "loud talker");
  let quiet = sentence_at(&mut gain, -22.0, -30.0);
  // The game lowered by the depth sits the margin under the quiet talker.
  assert!(((-22.0 - quiet) - (-30.0 - DEFAULT.strength * T.max_margin_db)).abs() < 0.5, "{quiet}");
}

#[test]
fn a_loud_talkers_soft_onset_does_not_raise_the_level() {
  let mut gain = DuckGain::default();
  drive(&mut gain, 1.0, -22.0, |_| false);
  let depths = drive_levels(&mut gain, 2.0, -22.0, |i| Some(if i < 5 { -35.0 } else { -6.0 }));
  assert_eq!((peak(&depths), gain.level_db), (0.0, 0.0));
}

#[test]
fn a_short_ok_over_a_fight_raises_nothing() {
  let mut gain = DuckGain::default();
  drive_voice(&mut gain, 0.1, -22.0, -30.0, |_| true); // shorter than `spurt_min_s`
  drive(&mut gain, T.hold_s + 1.0, -22.0, |_| false);
  assert_eq!(gain.level_db, 0.0);
}

#[test]
fn a_quiet_talker_answering_a_loud_one_ducks_within_a_second() {
  let mut gain = DuckGain::default();
  drive(&mut gain, 1.0, -22.0, |_| false);
  let loud = drive_voice(&mut gain, 4.0, -22.0, -12.0, |_| true);
  drive(&mut gain, 0.3, -22.0, |_| false); // within the hold: one spurt
  let quiet = drive_voice(&mut gain, 4.0, -22.0, -30.0, |_| true);
  assert_eq!(peak(&loud), 0.0, "the loud talker is above the game");
  assert!(quiet[100] > 5.0, "lowered one second in: {}", quiet[100]);
  let full = -22.0 + 30.0 + DEFAULT.strength * T.max_margin_db;
  assert!((quiet.last().unwrap() - full).abs() < 0.5, "{quiet:?}");
}

// --- Two levels: 0 while nobody talks, the lowering level while someone does ---

#[test]
fn the_first_fight_with_nothing_known_raises_the_level_only_to_what_it_needs() {
  // Regression: the level starts at 0 with every run and nothing is stored; the first fight's
  // sentence needs game - talker + margin and gets exactly that, not the bound.
  let mut gain = DuckGain::default();
  assert_eq!(drive(&mut gain, 30.0, -22.0, |_| false).last(), Some(&0.0), "nobody talks: nothing lowered");
  let first = sentence_at(&mut gain, -22.0, -30.0);
  let need = 8.0 + DEFAULT.strength * T.max_margin_db;
  assert!((first - need).abs() < 0.05 && (gain.level_db - need).abs() < 0.05, "{first} {}", gain.level_db);
  assert_eq!(gain.depth_db, 0.0, "back to 0 after the talk");
  assert_eq!(gain.raises, 1);
}

#[test]
fn a_quieter_fight_keeps_the_level() {
  // Regression: the level comes down only by the inaudible decay (or once the game is not heard
  // for a long time), not with a quieter fight.
  let mut gain = DuckGain::default();
  let loud = sentence_at(&mut gain, -22.0, -30.0);
  let quieter = sentence_at(&mut gain, -28.0, -30.0);
  let calm = sentence_at(&mut gain, -45.0, -30.0);
  let near = |a: f32| (a - loud).abs() < 0.1;
  assert!(loud > 10.0 && near(quieter) && near(calm), "{loud} {quieter} {calm}");
  // A talker louder than the game is lowered to the level too: two levels only.
  assert!(near(sentence_at(&mut gain, -45.0, -12.0)));
}

#[test]
fn a_louder_fight_raises_the_level_by_what_it_needs_more() {
  let mut gain = DuckGain::default();
  let first = sentence_at(&mut gain, -26.0, -30.0);
  let louder = sentence_at(&mut gain, -22.0, -30.0);
  assert!((louder - first - 4.0).abs() < 0.05, "{first} -> {louder}");
  assert_eq!(gain.raises, 2);
  assert!((sentence_at(&mut gain, -26.0, -30.0) - louder).abs() < 0.1, "kept");
}

#[test]
fn ten_minutes_of_audible_calm_lower_the_level_by_one_db_and_the_next_fight_raises_it_again() {
  let mut gain = DuckGain::default();
  let before = sentence_at(&mut gain, -22.0, -30.0);
  drive(&mut gain, 600.0, -60.0, |_| false);
  assert!((before - gain.level_db - 1.0).abs() < 0.05, "1 dB in 10 minutes: {before} -> {}", gain.level_db);
  // A calm sentence is lowered by the level it had less that dB: not noticed.
  let calm = sentence_at(&mut gain, -60.0, -30.0);
  assert!((before - calm - 1.0).abs() < 0.1, "{before} -> {calm}");
  // The same fight again needs the full level: raised by about that dB, the only audible move.
  let again = sentence_at(&mut gain, -22.0, -30.0);
  assert!((again - before).abs() < 0.05, "{before} -> {again}");
}

#[test]
fn a_long_silent_game_starts_the_level_over() {
  // Regression: a lobby or a menu (the game under `SILENT_GAME_DB`) for `SILENT_RESET_S`.
  let mut gain = DuckGain::default();
  let before = sentence_at(&mut gain, -22.0, -30.0);
  drive(&mut gain, SILENT_RESET_S - 5.0, -70.0, |_| false);
  assert!((gain.level_db - before).abs() < 0.2, "not yet: only the decay");
  // The game level falls under the threshold within about 15 s of the fight's end.
  drive(&mut gain, 30.0, -70.0, |_| false);
  assert_eq!((gain.level_db, gain.raises), (0.0, 0));
  // The next fight learns from 0 again, its own need only.
  let quieter = sentence_at(&mut gain, -28.0, -30.0);
  assert!((before - quieter - 6.0).abs() < 0.1, "{before} -> {quieter}");
}

#[test]
fn an_endless_talker_stays_lowered_while_the_game_is_heard() {
  // A podcast at -24 dB never pauses: one spurt. Loud game for 10 s, then calm but audible.
  let mut gain = DuckGain::default();
  let loud = drive_levels(&mut gain, 10.0, -22.0, |_| Some(-24.0));
  let calm = drive_levels(&mut gain, 40.0, -60.0, |_| Some(-24.0));
  let level = *loud.last().unwrap();
  assert!(level > 5.0 && calm.iter().all(|d| (d - level).abs() < 0.1), "{level}");
  // Silent long enough: lets go at the release rate.
  let silent = drive_levels(&mut gain, SILENT_RESET_S + 10.0, -80.0, |_| Some(-24.0));
  assert_eq!(*silent.last().unwrap(), 0.0);
}

#[test]
fn gaps_in_a_fight_do_not_pump() {
  // The endless talker over a fight with 4 s gaps: the depth stays at the level throughout.
  let mut gain = DuckGain::default();
  drive_levels(&mut gain, 4.0, -22.0, |_| Some(-24.0));
  let level = gain.level_db;
  for _ in 0..4 {
    for game_db in [-60.0, -22.0] {
      // Within 0.1 dB: the level decays inaudibly in the gaps (1 dB in 10 minutes).
      assert!(drive_levels(&mut gain, 4.0, game_db, |_| Some(-24.0)).iter().all(|d| (d - level).abs() < 0.1));
    }
  }
}

#[test]
fn the_same_scenario_at_10_and_50_ms_steps_ends_at_the_same_depth() {
  // The service's mix ducker steps 20-100 ms: sentences over a fight, 2 minutes.
  let run = |dt: f32| {
    let mut gain = DuckGain::default();
    let (mut peak, mut last_peak) = (0.0f32, 0.0f32);
    for step in 0..(120.0 / dt).round() as usize {
      let t = step as f32 * dt % 7.0;
      let in_sentence = (1.0..4.0).contains(&t) && (t - 1.0) % 0.7 < 0.5;
      let chat_ms = if in_sentence { 10f32.powf(-3.0) } else { 0.0 };
      let depth = gain.update(dt, chat_ms, 10f32.powf(-2.2), &DEFAULT, &T);
      if (1.0..4.0).contains(&t) {
        peak = peak.max(depth);
      } else if peak > 0.0 {
        (last_peak, peak) = (peak, 0.0); // the sentence is over
      }
    }
    last_peak
  };
  let (fine, coarse) = (run(0.01), run(0.05));
  assert!(fine > 5.0 && (fine - coarse).abs() < 0.5, "{fine} vs {coarse}");
}

// --- Starting over ---

#[test]
fn a_volume_change_starts_the_level_over() {
  let mut gain = DuckGain::default();
  let before = sentence_at(&mut gain, -22.0, -30.0);
  // A session volume: the game turned down 6 dB, its measured level moves with it.
  gain.shift(-6.0, 0.0);
  assert_eq!(gain.level_db, 0.0);
  let game_down = sentence_at(&mut gain, -28.0, -30.0);
  assert!((before - game_down - 6.0).abs() < 0.1, "{before} -> {game_down}");
  // A channel output's volume (in the settings): the same.
  let half = DuckSettings { game_gain: 0.5, ..DEFAULT };
  drive_at(&mut gain, &half, 0.01, -28.0, -30.0, |_| false);
  assert_eq!(gain.level_db, 0.0);
  let quieter = peak(&drive_at(&mut gain, &half, 3.0, -28.0, -30.0, |i| i % 70 < 50));
  assert!((game_down - quieter - 6.02).abs() < 0.1, "{game_down} -> {quieter}");
}

#[test]
fn a_restart_or_the_feature_off_starts_the_level_over() {
  let mut gain = DuckGain::default();
  sentence_at(&mut gain, -22.0, -30.0);
  gain.restart();
  assert_eq!((gain.level_db, gain.depth_db), (0.0, 0.0));
  sentence_at(&mut gain, -22.0, -30.0);
  let off = DuckSettings { enabled: false, ..DEFAULT };
  drive_at(&mut gain, &off, 0.01, -22.0, -30.0, |_| true);
  assert_eq!(gain.level_db, 0.0);
}

#[test]
fn a_depth_still_applied_when_the_level_starts_over_comes_back_at_the_release_rate() {
  let mut gain = DuckGain::default();
  let lowered = drive_levels(&mut gain, 4.0, -22.0, |_| Some(-30.0));
  gain.shift(0.0, -1.0);
  let after = drive_levels(&mut gain, 0.5, -22.0, |_| None);
  let fell = lowered.last().unwrap() - after.last().unwrap();
  assert!((fell - T.release_db_per_s * 0.5).abs() < 0.05, "{fell}");
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

/// A few seconds of game (the game level settles), then a 3 s sentence (one spurt, 2.2 s of talk)
/// at `voice_db`, then the hold and the whole release: the sentence's peak depth.
fn sentence_at(gain: &mut DuckGain, game_db: f32, voice_db: f32) -> f32 {
  drive(gain, 5.0, game_db, |_| false);
  let depth = peak(&drive_voice(gain, 3.0, game_db, voice_db, |i| i % 70 < 50));
  drive(gain, T.hold_s + T.max_depth_db / T.release_db_per_s + 0.5, game_db, |_| false);
  depth
}

fn peak(depths: &[f32]) -> f32 {
  depths.iter().fold(0.0f32, |m, v| m.max(*v))
}
