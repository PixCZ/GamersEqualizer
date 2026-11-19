use super::*;
use crate::ducking::gain::SILENT_RESET_S;
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
    self.depth = self.ducker.process(&mix, TICK_S, (speech_ms, band_ms), sounding, &DEFAULT, &self.tuning);
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
fn chat_alone_in_the_mix_never_ducks_even_3_db_louder_than_at_its_stream() {
  let mut rig = MixRig::new();
  rig.chat_in_mix = 1.4;
  rig.chat_late_ticks = 1;
  for _ in 0..10 {
    let depths: Vec<f32> = [rig.sentence(5.0, 0.0), rig.run(2.0, 0.0, 0.0)].concat();
    assert!(depths.iter().all(|d| *d == 0.0), "{depths:?}");
  }
}

#[test]
fn a_fight_running_when_speech_starts_ducks_to_the_lowering_level_and_holds_the_game() {
  let mut rig = MixRig::new();
  rig.run(20.0, FIGHT, 0.0);
  let game = rig.game_db();
  let depths = rig.run(5.0, FIGHT, VOICE);
  let max = rig.tuning.max_depth_db;
  assert!(depths.iter().all(|d| *d <= max) && depths[40..].iter().all(|d| *d == max), "{depths:?}");
  assert!((rig.game_db() - game).abs() < 0.01, "held: {game} -> {}", rig.game_db());
}

#[test]
fn a_game_going_silent_in_speech_lets_go_only_after_the_long_silence() {
  let mut rig = MixRig::new();
  rig.run(20.0, FIGHT, 0.0);
  rig.run(5.0, FIGHT, VOICE);
  let (game, max) = (rig.game_db(), rig.tuning.max_depth_db);
  // Silent, but the talker does not pause: not noticed.
  let unnoticed = rig.run(10.0, 0.0, VOICE);
  assert!((rig.game_db() - game).abs() < 0.01 && unnoticed.iter().all(|d| (d - max).abs() < 0.1));
  // 0.4 s pauses: longer than the guard, shorter than the hold (still one spurt); each one measures
  // the silent game and its level falls slowly towards it. The level holds until the game has not
  // been heard for `SILENT_RESET_S`, then the game comes back slowly.
  let after: Vec<f32> = (0..50).flat_map(|_| [rig.run(0.4, 0.0, 0.0), rig.run(3.0, 0.0, VOICE)].concat()).collect();
  let slowest = rig.tuning.release_db_per_s * TICK_S + 1e-4;
  assert!(after.windows(2).all(|w| w[0] - w[1] <= slowest), "the whole game comes back slowly");
  let released = after.iter().position(|d| *d < max - 0.5).expect("released while the talk goes on");
  assert!(released as f32 * TICK_S >= SILENT_RESET_S, "{released}");
  assert_eq!(*after.last().unwrap(), 0.0);
  assert!(rig.ducker.status().spurt_talk_s > 100.0, "one spurt throughout");
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
fn a_lull_between_fights_keeps_the_duck_while_the_talk_goes_on() {
  // Regression (live log 2026-11-19): a podcast ran on, a calm stretch let go of the duck and the
  // next fight needed two pauses to lower the game again; a sentence was lost.
  let mut rig = MixRig::new();
  rig.run(20.0, FIGHT, 0.0);
  rig.run(5.0, FIGHT, VOICE);
  let max = rig.tuning.max_depth_db;
  let lull: Vec<f32> = (0..12).flat_map(|_| [rig.run(3.0, 0.0, VOICE), rig.run(0.4, 0.0, 0.0)].concat()).collect();
  assert!(lull.iter().all(|d| (d - max).abs() < 0.2), "{lull:?}");
  assert!(rig.run(1.0, FIGHT, VOICE).iter().all(|d| (d - max).abs() < 0.2), "the next fight finds it lowered");
}

#[test]
fn the_next_talk_starts_at_the_level_while_the_game_is_heard() {
  // The user: when the talk stops the game comes back; when it starts again it goes straight back to
  // the reduction of the talk before (a paused podcast played again in a calm moment of a fight
  // waited for chat pauses to measure the fight again).
  let mut rig = MixRig::new();
  rig.run(20.0, FIGHT, 0.0);
  rig.run(5.0, FIGHT, VOICE);
  let max = rig.tuning.max_depth_db;
  assert_eq!(*rig.run(60.0, QUIET_GAME, 0.0).last().unwrap(), 0.0, "nobody talks: the game comes back");
  assert!((rig.run(1.0, QUIET_GAME, VOICE).last().unwrap() - max).abs() < 0.3, "talk again: the level at once");
  // A game not heard for long (a menu): the level starts over, the next talk is not lowered.
  rig.run(150.0, 0.0, 0.0);
  assert_eq!(rig.ducker.status().level_db, 0.0);
  assert!(rig.run(3.0, 0.0, VOICE).iter().all(|d| *d == 0.0));
}

#[test]
fn a_fight_starting_in_speech_is_picked_up_without_any_pause() {
  // Live log 2026-11-19: a fight that began in the middle of a podcast sentence lowered the game
  // 5-10 s late, when the next pause let the mix be measured.
  let mut rig = MixRig::new();
  rig.run(20.0, FIGHT, 0.0); // an earlier fight nobody talked over
  rig.run(40.0, 0.0, 0.0); // the held game level falls slowly: long enough to be gone
  rig.run(5.0, 0.0, VOICE);
  let depths = rig.run(3.0, FIGHT, VOICE);
  assert!(depths.iter().take(10).all(|d| *d == 0.0), "one shot's time is not a fight: {depths:?}");
  assert!(*depths.last().unwrap() > 5.0, "{depths:?}");
}

#[test]
fn one_shot_in_a_calm_game_does_not_lower_it_while_the_talk_goes_on() {
  // Regression: a pause's mean became the held game level at once, so one shot in a pause of a
  // calm game was held as a fight and lowered the game for about 25 s while the talk went on.
  let mut rig = MixRig::new();
  rig.run(20.0, FIGHT, 0.0);
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
fn the_chats_tail_in_the_mix_does_not_read_as_game() {
  // A chat 3 dB louder in the mix arrives 0.1 s late: its tail is still there after it stopped
  // sounding; the guard and the held chat level keep it out of the game.
  let mut rig = MixRig::new();
  (rig.chat_in_mix, rig.chat_late_ticks) = (1.4, 2);
  rig.run(20.0, QUIET_GAME, 0.0);
  rig.run(3.0, QUIET_GAME, VOICE);
  let before = rig.game_db();
  rig.run(0.1, QUIET_GAME, 0.0);
  rig.run(1.0, QUIET_GAME, VOICE);
  rig.run(0.1, QUIET_GAME, 0.0);
  assert!((rig.game_db() - before).abs() < 0.01, "{before} -> {}", rig.game_db());
}

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
fn the_chat_in_the_mix_never_raises_the_level() {
  // The chat 3 dB louder in the mix than at its stream: the level is what the game alone needs.
  let mut rig = MixRig::new();
  rig.chat_in_mix = 1.4;
  rig.run(20.0, QUIET_GAME, 0.0);
  rig.run(10.0, QUIET_GAME, VOICE);
  let st = rig.ducker.status();
  assert!(st.need_db.is_some_and(|need| (st.level_db - need).abs() < 0.5), "{st:?}");
  assert!(st.level_db > 2.0 && st.level_db < 12.0, "{st:?}");
}

#[test]
fn a_fight_heard_only_in_short_pauses_raises_the_level_after_the_reduction_starts() {
  // Regression: the game was learned only with no reduction applied, so a fight that began with
  // the talk was heard for one tick, then lowered, and the depth stayed sized for the game before.
  let mut rig = MixRig::new();
  rig.run(20.0, 0.0, 0.0);
  for _ in 0..20 {
    rig.run(3.0, FIGHT, VOICE);
    rig.run(0.4, FIGHT, 0.0);
  }
  let st = rig.ducker.status();
  assert!((st.level_db - rig.tuning.max_depth_db).abs() < 0.2 && st.depth_db > 5.0, "{st:?}");
}

#[test]
fn a_fight_growing_louder_while_lowered_raises_the_level() {
  // Regression (live log 2026-11-19): a fight that began moderate and grew louder under the
  // reduction was never learned and the depth stayed sized for its start.
  let mut rig = MixRig::new();
  rig.run(20.0, QUIET_GAME, 0.0);
  rig.run(5.0, QUIET_GAME, VOICE);
  let start = rig.ducker.status();
  for _ in 0..20 {
    rig.run(3.0, FIGHT, VOICE);
    rig.run(0.4, FIGHT, 0.0);
  }
  let st = rig.ducker.status();
  assert!(start.depth_db > 0.5 && st.depth_db > start.depth_db + 5.0, "{} -> {}", start.depth_db, st.depth_db);
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
    rig.ducker.process(&[], TICK_S, (0.0, 0.0), false, &DEFAULT, &rig.tuning);
  }
  assert!((rig.game_db() - before).abs() < 0.01);
}

#[test]
fn it_starts_unlowered_and_a_session_volume_change_starts_the_level_over() {
  let mut rig = MixRig::new();
  assert_eq!(rig.ducker.status().level_db, 0.0, "nothing known at the start");
  rig.run(20.0, FIGHT, 0.0);
  rig.run(5.0, FIGHT, VOICE);
  let game = rig.game_db();
  rig.ducker.shift(-6.0, 0.0);
  let st = rig.ducker.status();
  assert!(st.level_db == 0.0 && (st.game_db - game + 6.0).abs() < 0.01, "{st:?}");
  // The depth comes back at the release rate, then the talk raises the level again at once.
  let next = rig.tick(FIGHT, 0.0);
  assert!((rig.tuning.max_depth_db - next - rig.tuning.release_db_per_s * TICK_S).abs() < 1e-3, "{next}");
  // The rig does not lower its game by the volume: the held level reads 6 dB low, and so the need.
  rig.run(3.0, FIGHT, VOICE);
  let level = rig.ducker.status().level_db;
  assert!(level > 10.0 && level < rig.tuning.max_depth_db, "{level}");
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
