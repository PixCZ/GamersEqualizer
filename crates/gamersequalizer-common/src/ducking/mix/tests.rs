use super::*;
use crate::streams::StreamInfo;
use std::collections::VecDeque;

const RATE: f32 = 48_000.0;
/// The service's tick.
const TICK_S: f32 = 0.05;
/// 1.5 kHz game tone: firefight about -9 dB in the band, a quieter game about -23 dB.
const FIGHT: f32 = 0.5;
const QUIET_GAME: f32 = 0.1;
/// 1.2 kHz chat voice: about -23 dB in the band.
const VOICE: f32 = 0.1;
const DEFAULT: DuckSettings = DuckSettings { enabled: true, strength: 0.5, game_gain: 1.0, chat_gain: 1.0 };

/// The service's loop: the mix is the game at the volume the ducker asked for (by default one tick
/// ago) plus the chat as it arrives in the mix; the chat's speech level and whether it sounds come
/// from its stream.
struct MixRig {
  ducker: MixDucker,
  tuning: DuckTuning,
  chat_band: BandPass,
  chat_state: [[f64; 2]; 1],
  phase: usize,
  depth: f32,
  /// The depths returned so far, and how many ticks late the caller applies one (None = never).
  returned: VecDeque<f32>,
  applied_after: Option<usize>,
  /// The chat in the mix: its amplitude against its stream, and how many ticks it arrives late.
  chat_in_mix: f32,
  chat_late_ticks: usize,
  chat_history: VecDeque<Vec<f32>>,
}

impl MixRig {
  fn new() -> Self {
    let tuning = DuckTuning::DEFAULT;
    MixRig {
      ducker: MixDucker::new(RATE),
      tuning,
      chat_band: BandPass::new(f64::from(RATE), &tuning),
      chat_state: [[0.0; 2]; 1],
      phase: 0,
      depth: 0.0,
      returned: VecDeque::new(),
      applied_after: Some(1),
      chat_in_mix: 1.0,
      chat_late_ticks: 0,
      chat_history: VecDeque::new(),
    }
  }

  /// One tick; returns the depth to apply from the next tick on.
  fn tick(&mut self, game: f32, voice: f32) -> f32 {
    let frames = (TICK_S * RATE) as usize;
    let tone = |f: f32, a: f32, i: usize| a * (2.0 * std::f32::consts::PI * f * (self.phase + i) as f32 / RATE).sin();
    let applied =
      self.applied_after.and_then(|late| self.returned.len().checked_sub(late)).map_or(0.0, |i| self.returned[i]);
    let gain = 10f32.powf(-applied / 20.0);
    let chat: Vec<f32> = (0..frames).map(|i| tone(1200.0, voice, i)).collect();
    self.chat_history.push_back(chat.clone());
    let late = self.chat_history.len().checked_sub(1 + self.chat_late_ticks).map(|i| self.chat_history[i].clone());
    let in_mix = late.unwrap_or_else(|| vec![0.0; frames]);
    let mix: Vec<f32> =
      in_mix.iter().enumerate().flat_map(|(i, c)| [tone(1500.0, game, i) * gain + c * self.chat_in_mix; 2]).collect();
    self.phase += frames;
    // As the stream APO's envelope: the peak per block, at the stream.
    let sounding = chat.iter().any(|s| s.abs() > 10f32.powf(DuckTuning::DEFAULT.mix_chat_sound_db / 20.0));
    let band_ms = self.chat_band.measure(&mut self.chat_state, &chat, 1);
    let speech_ms = if voice > 0.0 { band_ms } else { 0.0 };
    self.depth = self.ducker.process(&mix, TICK_S, speech_ms, sounding, &DEFAULT, &self.tuning);
    self.returned.push_back(self.depth);
    self.depth
  }

  /// `seconds` of ticks; the depths.
  fn run(&mut self, seconds: f32, game: f32, voice: f32) -> Vec<f32> {
    (0..(seconds / TICK_S).round() as usize).map(|_| self.tick(game, voice)).collect()
  }

  /// Words of 0.5 s with 0.2 s pauses (pauses inside the hold and the guard: one spurt).
  fn sentence(&mut self, seconds: f32, game: f32) -> Vec<f32> {
    (0..(seconds / TICK_S).round() as usize)
      .map(|i| self.tick(game, if (i as f32 * TICK_S) % 0.7 < 0.5 { VOICE } else { 0.0 }))
      .collect()
  }

  fn game_db(&self) -> f32 {
    self.ducker.status().game_db
  }
}

#[test]
fn chat_alone_in_the_mix_never_ducks_even_20_db_louder_than_at_its_stream() {
  let mut rig = MixRig::new();
  rig.chat_in_mix = 10.0;
  rig.chat_late_ticks = 1;
  for _ in 0..10 {
    let depths: Vec<f32> = [rig.sentence(5.0, 0.0), rig.run(2.0, 0.0, 0.0)].concat();
    assert!(depths.iter().all(|d| *d == 0.0), "{depths:?}");
  }
}

#[test]
fn a_fight_running_when_speech_starts_ducks_to_the_locked_depth_and_holds_the_game() {
  let mut rig = MixRig::new();
  rig.run(20.0, FIGHT, 0.0);
  let game = rig.game_db();
  let depths = rig.run(5.0, FIGHT, VOICE);
  let max = rig.tuning.mix_max_depth_db;
  assert!(depths.iter().all(|d| *d <= max) && depths[40..].iter().all(|d| *d == max), "{depths:?}");
  assert!((rig.game_db() - game).abs() < 0.01, "held: {game} -> {}", rig.game_db());
}

#[test]
fn a_game_going_calm_in_speech_is_followed_at_the_first_pause_longer_than_the_guard() {
  let mut rig = MixRig::new();
  rig.run(20.0, FIGHT, 0.0);
  rig.run(5.0, FIGHT, VOICE);
  let game = rig.game_db();
  // Calm, but the talker does not pause: not noticed.
  let unnoticed = rig.run(10.0, 0.0, VOICE);
  assert!((rig.game_db() - game).abs() < 0.01 && unnoticed.iter().all(|d| *d == rig.tuning.mix_max_depth_db));
  // 0.4 s pauses: longer than the guard, shorter than the hold (still one spurt); each one measures
  // the calm game and the level falls slowly towards it.
  let after: Vec<f32> = (0..12).flat_map(|_| [rig.run(0.4, 0.0, 0.0), rig.run(3.0, 0.0, VOICE)].concat()).collect();
  let slowest = rig.tuning.mix_release_db_per_s * TICK_S + 1e-4;
  assert!(after.windows(2).all(|w| w[0] - w[1] <= slowest), "the whole game comes back slowly");
  let released = after.iter().position(|d| *d == 0.0).expect("released while the talk goes on");
  assert!(released as f32 * TICK_S >= rig.tuning.latch_release_s, "{released}");
  assert!(rig.ducker.status().spurt_talk_s > 20.0, "one spurt throughout");
}

#[test]
fn a_few_quiet_seconds_between_fights_do_not_let_go_of_the_duck_while_the_talk_goes_on() {
  let mut rig = MixRig::new();
  rig.run(20.0, FIGHT, 0.0);
  rig.run(5.0, FIGHT, VOICE);
  let ducked = *rig.run(1.0, FIGHT, VOICE).last().unwrap();
  assert!(ducked > 5.0);
  // The game goes quiet for 6 s; the talker's pauses (longer than the guard) measure it.
  let quiet: Vec<f32> =
    (0..3).flat_map(|_| [rig.run(1.5, QUIET_GAME, VOICE), rig.run(0.4, QUIET_GAME, 0.0)].concat()).collect();
  assert!(quiet.iter().all(|d| *d >= ducked - 0.5), "{quiet:?}");
}

#[test]
fn a_fight_starting_in_speech_is_picked_up_at_the_second_pause() {
  let mut rig = MixRig::new();
  rig.run(20.0, FIGHT, 0.0); // an earlier fight: the loud level is known
  rig.run(40.0, 0.0, 0.0); // the level falls slowly: long enough to be gone
  rig.run(5.0, 0.0, VOICE);
  assert!(rig.run(5.0, FIGHT, VOICE).iter().all(|d| *d == 0.0), "not while the talk goes on");
  rig.run(0.4, FIGHT, 0.0);
  assert!(rig.run(1.0, FIGHT, VOICE).iter().all(|d| *d == 0.0), "one pause could be one shot");
  rig.run(0.4, FIGHT, 0.0);
  assert!(*rig.run(3.0, FIGHT, VOICE).last().unwrap() > 5.0);
}

#[test]
fn one_shot_in_a_calm_game_does_not_lower_it_while_the_talk_goes_on() {
  // Regression: a pause's mean became the held game level at once, so one shot in a pause of a
  // calm game was held as a fight and lowered the game for about 25 s while the talk went on.
  let mut rig = MixRig::new();
  rig.run(20.0, FIGHT, 0.0); // the loud level is known
  rig.run(40.0, 0.0, 0.0);
  let calm = rig.game_db();
  let mut depths = rig.run(3.0, 0.0, VOICE);
  depths.extend([rig.run(0.3, 0.0, 0.0), rig.run(0.1, FIGHT, 0.0), rig.run(3.0, 0.0, VOICE)].concat());
  for _ in 0..5 {
    depths.extend([rig.run(0.4, 0.0, 0.0), rig.run(3.0, 0.0, VOICE)].concat());
  }
  assert!(depths.iter().all(|d| *d == 0.0), "{depths:?}");
  assert!(rig.game_db() <= calm, "{calm} -> {}", rig.game_db());
}

#[test]
fn the_guard_keeps_the_chats_tail_out_of_the_game() {
  // A loud chat arrives in the mix 0.1 s late: its tail is still there after it stopped sounding.
  let tail = |guard: f32| {
    let mut rig = MixRig::new();
    assert!(rig.tuning.set("mix_chat_guard_s", guard));
    (rig.chat_in_mix, rig.chat_late_ticks) = (10.0, 2);
    rig.run(20.0, QUIET_GAME, 0.0);
    rig.run(3.0, QUIET_GAME, VOICE);
    let before = rig.game_db();
    // Two pauses: the held level rises only to what two segments in a row read.
    rig.run(0.1, QUIET_GAME, 0.0);
    rig.run(1.0, QUIET_GAME, VOICE);
    rig.run(0.1, QUIET_GAME, 0.0);
    rig.game_db() - before
  };
  assert!(tail(T_GUARD).abs() < 0.01, "{}", tail(T_GUARD));
  assert!(tail(0.05) > 3.0, "without the guard the tail reads as game: {}", tail(0.05));
}

const T_GUARD: f32 = DuckTuning::DEFAULT.mix_chat_guard_s;

#[test]
fn its_own_reduction_is_undone_during_the_release_with_the_depth_applied_late() {
  let mut rig = MixRig::new();
  rig.applied_after = Some(4); // 200 ms late
  rig.run(20.0, FIGHT, 0.0);
  let game = rig.game_db();
  for _ in 0..4 {
    rig.sentence(3.0, FIGHT);
    for _ in 0..40 {
      rig.tick(FIGHT, 0.0); // the release ramp, measured
      assert!((rig.game_db() - game).abs() < 1.0, "{game} -> {}", rig.game_db());
    }
  }
}

#[test]
fn the_chat_never_teaches_the_loud_level() {
  let mut rig = MixRig::new();
  rig.chat_in_mix = 10.0;
  rig.run(20.0, FIGHT, 0.0);
  let loud = rig.ducker.status().loud_db.unwrap();
  rig.run(10.0, FIGHT, VOICE);
  assert_eq!(rig.ducker.status().loud_db.unwrap(), loud);
}

#[test]
fn a_fight_heard_only_in_short_pauses_is_learned_after_the_reduction_starts() {
  // Regression: the loud level learned only with no reduction applied, and one measurement counted
  // as one tick, so a fight that began with the talk was learned for one tick, then lowered, and
  // the depth stayed sized for whatever the game was before.
  let mut rig = MixRig::new();
  rig.run(20.0, 0.0, 0.0);
  for _ in 0..20 {
    rig.run(3.0, FIGHT, VOICE);
    rig.run(0.4, FIGHT, 0.0);
  }
  let st = rig.ducker.status();
  let loud = st.loud_db.unwrap();
  assert!(st.depth_db > 5.0 && loud > st.game_db - 3.0, "loud {loud}, game {}, depth {}", st.game_db, st.depth_db);
}

#[test]
fn a_reduction_that_never_arrives_cannot_inflate_the_loud_level() {
  // The undo lifts whatever the service does not lower (here: the volume change never happens);
  // learning it would raise the depth, then the level, up to the bound, and store it.
  let mut rig = MixRig::new();
  rig.applied_after = None;
  rig.run(20.0, FIGHT, 0.0);
  let loud = rig.ducker.status().loud_db.unwrap();
  for _ in 0..20 {
    rig.run(3.0, FIGHT, VOICE);
    rig.run(0.4, FIGHT, 0.0);
  }
  let st = rig.ducker.status();
  assert!(st.depth_db > 5.0 && st.loud_db.unwrap() <= loud + 0.5, "{loud} -> {:?}", st.loud_db);
}

#[test]
fn a_segment_from_long_ago_confirms_no_shot() {
  // After a minute of talk the segment before is the fight that ended meanwhile: aged, it must not
  // let one shot in the next pause pass for the fight going on.
  let mut rig = MixRig::new();
  rig.run(20.0, FIGHT, 0.0);
  let fight = rig.game_db();
  rig.run(60.0, 0.0, VOICE);
  rig.run(0.6, 0.0, 0.0); // the guard, then the calm game measured: the held level falls to it
  rig.run(0.1, FIGHT, 0.0); // one shot, same pause
  rig.run(10.0, 0.0, VOICE);
  assert!(rig.game_db() < fight - 20.0, "fight {fight}, now {}", rig.game_db());
}

#[test]
fn a_resync_mid_duck_keeps_the_depth() {
  let mut rig = MixRig::new();
  rig.run(20.0, FIGHT, 0.0);
  let depth = *rig.run(5.0, FIGHT, VOICE).last().unwrap();
  assert!(depth > 5.0);
  rig.ducker.resync();
  let next = rig.tick(FIGHT, VOICE);
  let step = rig.tuning.fast_attack_db_per_s.max(rig.tuning.release_db_per_s) * TICK_S;
  assert!((next - depth).abs() <= step + 1e-4, "{depth} -> {next}");
}

#[test]
fn ticks_without_a_reference_hold_the_estimate() {
  let mut rig = MixRig::new();
  rig.run(10.0, FIGHT, 0.0);
  let before = rig.game_db();
  for _ in 0..40 {
    rig.ducker.process(&[], TICK_S, 0.0, false, &DEFAULT, &rig.tuning);
  }
  assert!((rig.game_db() - before).abs() < 0.01);
}

#[test]
fn stored_levels_give_the_locked_depth_from_the_first_sentence() {
  let mut rig = MixRig::new();
  rig.run(1.0, QUIET_GAME, 0.0);
  // The quieter game (-23.0 dB in the band) and a quiet talker stored at -23.2 dB.
  rig.ducker.seed(Some(-23.0), Some(-23.2));
  rig.run(3.0, QUIET_GAME, 0.0);
  let first = rig.sentence(3.0, QUIET_GAME).iter().fold(0.0f32, |m, d| m.max(*d));
  let target = 0.2 + DEFAULT.strength * rig.tuning.mix_max_margin_db;
  assert!((first - target).abs() < 0.1, "{first} vs {target}");
  let st = rig.ducker.status();
  assert!(st.loud_seeded && st.quiet_seeded);
}

#[test]
fn chat_sounding_reads_the_fresh_chat_envelopes_within_the_window() {
  let now = 5_000_000_000;
  // (bus, (time before now, peak) recorded) -> sounding within 0.1 s
  type Case = (StreamBus, &'static [(u64, f32)], bool);
  let cases: [Case; 5] = [
    (StreamBus::Chat, &[(0, 0.01)], true),
    (StreamBus::Chat, &[(3_000_000, 0.01), (0, 0.0001)], false), // the peak 0.3 s ago
    (StreamBus::Chat, &[(10_000_000, 0.01)], false),             // stale: nothing for a second
    (StreamBus::Game, &[(0, 0.5)], false),
    (StreamBus::Chat, &[(0, 0.0001)], false), // below the threshold
  ];
  for (bus, records, sounding) in cases {
    let table = table();
    let index = table.claim(&StreamInfo { pid: 1, now_hns: 0, sample_rate: 48_000, channels: 2, mode: 0 }).unwrap().0;
    let slot = &table.slots[index];
    slot.bus.store(bus.to_u32(), Ordering::Relaxed);
    for (ago, peak) in records {
      slot.record(now - ago, *peak, 480);
    }
    assert_eq!(chat_sounding(&table, now, 0.1, DuckTuning::DEFAULT.mix_chat_sound_db), sounding, "{bus:?} {records:?}");
  }
  // The peak 0.3 s ago counts in a 0.5 s window.
  let table = table();
  let index = table.claim(&StreamInfo { pid: 1, now_hns: 0, sample_rate: 48_000, channels: 2, mode: 0 }).unwrap().0;
  table.slots[index].bus.store(StreamBus::Chat.to_u32(), Ordering::Relaxed);
  table.slots[index].record(now - 3_000_000, 0.01, 480);
  table.slots[index].record(now, 0.0001, 480);
  assert!(chat_sounding(&table, now, 0.5, DuckTuning::DEFAULT.mix_chat_sound_db));
  // A breath at -50 dBFS (0.003) is no sound at the default threshold, but is at -60.
  table.slots[index].record(now, 0.003, 480);
  assert!(!chat_sounding(&table, now, 0.1, DuckTuning::DEFAULT.mix_chat_sound_db));
  assert!(chat_sounding(&table, now, 0.1, -60.0));
}

#[test]
fn chat_levels_sum_the_fresh_chat_slots() {
  let table = table();
  let now = 5_000_000_000;
  let info = StreamInfo { pid: 1, now_hns: 0, sample_rate: 48_000, channels: 2, mode: 0 };
  // (bus, last block, speech level, band level dB x 10)
  let slots = [
    (StreamBus::Chat, now, 0.01, -200),             // fresh: -20 dB band
    (StreamBus::Chat, now, 0.02, dbg::NA),          // fresh, no band level
    (StreamBus::Chat, now - 10_000_000, 1.0, -100), // stale: one second old
    (StreamBus::Game, now, 1.0, -100),              // not chat
  ];
  for (bus, last, speech, band) in slots {
    let index = table.claim(&info).map(|c| c.0).unwrap();
    let slot = &table.slots[index];
    slot.bus.store(bus.to_u32(), Ordering::Relaxed);
    slot.last_audio_hns.store(last, Ordering::Relaxed);
    slot.speech_ms.store((speech as f32).to_bits(), Ordering::Relaxed);
    slot.duck_dbg[dbg::BAND_DB].store(band, Ordering::Relaxed);
  }
  let (speech, band) = chat_levels(&table, now);
  assert!((speech - 0.03).abs() < 1e-6 && (band - 0.01).abs() < 1e-6, "{speech} {band}");
}

/// A fresh stream table, as the APO's mapping starts.
fn table() -> Box<StreamTable> {
  let mut mem: Box<std::mem::MaybeUninit<StreamTable>> = Box::new_uninit();
  // SAFETY: all-zero bytes are a valid StreamTable (atomics only), like a fresh mapping.
  unsafe {
    std::ptr::write_bytes(mem.as_mut_ptr(), 0, 1);
    mem.assume_init()
  }
}
