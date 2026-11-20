use super::*;
use crate::streams::StreamInfo;
use std::sync::atomic::Ordering;

const GAME: usize = 0;
const CHAT: usize = 1;
/// 1 kHz chat voice: about -24 dB in the band (`LOUD_VOICE` about -18 dB).
const VOICE: f32 = 0.1;
const LOUD_VOICE: f32 = 0.2;
/// 1.5 kHz game tone: firefight about -9 dB in the band, calm about -49 dB.
const LOUD: f32 = 0.5;
const CALM: f32 = 0.005;
const FIGHT: &[(f64, f32)] = &[(1500.0, LOUD)];
/// Strength 500.
const DEFAULT: DuckSettings = DuckSettings { enabled: true, strength: 0.5, game_gain: 1.0, chat_gain: 1.0 };
const T: DuckTuning = DuckTuning::DEFAULT;
/// Long enough for the whole depth to come back at the release rate.
const RELEASE_S: f32 = T.max_depth_db / T.release_db_per_s + 0.5;

/// A game stream and a chat stream in one table, stereo blocks.
struct Rig {
  table: Box<StreamTable>,
  game: StreamDucker,
  chat: StreamDucker,
  settings: DuckSettings,
  /// What the chat's voice detector says, and the game stream's role.
  speech: bool,
  role: DuckRole,
  rate: f64,
  frames: usize,
  now: u64,
  phase: usize,
}

impl Rig {
  fn new() -> Self {
    Self::at(48_000.0, 480)
  }

  fn at(rate: f64, frames: usize) -> Self {
    let mut mem: Box<std::mem::MaybeUninit<StreamTable>> = Box::new_uninit();
    // SAFETY: all-zero bytes are a valid StreamTable (atomics only), like a fresh mapping.
    let table = unsafe {
      std::ptr::write_bytes(mem.as_mut_ptr(), 0, 1);
      mem.assume_init()
    };
    // Claimed like the APO does (fresh telemetry reads `dbg::NA`), then paired.
    let info = StreamInfo { pid: 7, now_hns: 0, sample_rate: rate as u32, channels: 2, mode: 0 };
    assert_eq!([table.claim(&info).map(|c| c.0), table.claim(&info).map(|c| c.0)], [Some(GAME), Some(CHAT)]);
    table.slots[GAME].bus.store(StreamBus::Game.to_u32(), Ordering::Relaxed);
    table.slots[CHAT].bus.store(StreamBus::Chat.to_u32(), Ordering::Relaxed);
    let settings = DEFAULT;
    let (game, chat) = (StreamDucker::new(rate as f32), StreamDucker::new(rate as f32));
    let role = DuckRole::Ducked;
    Rig { table, game, chat, settings, speech: true, role, rate, frames, now: 1_000_000_000, phase: 0 }
  }

  /// One block: the chat says a 1 kHz tone at `voice` (0 = its stream is not called), the game
  /// plays `tones` (Hz, amplitude; none = digital zeros). Returns the game's input and output.
  fn block(&mut self, voice: f32, tones: &[(f64, f32)]) -> (Vec<f32>, Vec<f32>) {
    let dry = self.signal(tones);
    let mut out = dry.clone();
    self.feed(voice, &mut out);
    (dry, out)
  }

  /// Runs the game stream on `game` (stereo, one block) while the chat says `voice`.
  fn feed(&mut self, voice: f32, game: &mut [f32]) {
    self.now += (self.frames as f64 * 1e7 / self.rate).round() as u64;
    if voice > 0.0 {
      let dry = self.signal(&[(1000.0, voice)]);
      let mut out = dry.clone();
      self.table.slots[CHAT].last_audio_hns.store(self.now, Ordering::Relaxed);
      self.chat.process(&mut out, 2, DuckRole::Voice, self.speech, &self.table, CHAT, self.now, &self.settings);
      assert_eq!(out, dry, "the voice is never changed");
    }
    self.game.process(game, 2, self.role, false, &self.table, GAME, self.now, &self.settings);
    self.phase += self.frames;
  }

  /// Blocks for `seconds`; the game depth after each.
  fn run(&mut self, seconds: f32, voice: f32, tones: &[(f64, f32)]) -> Vec<f32> {
    (0..self.blocks(seconds))
      .map(|_| {
        self.block(voice, tones);
        self.depth()
      })
      .collect()
  }

  /// Words of 0.5 s with 0.2 s pauses.
  fn sentence(&mut self, seconds: f32, voice: f32, tones: &[(f64, f32)]) -> Vec<f32> {
    (0..self.blocks(seconds))
      .map(|i| {
        let talking = (i as f32 * self.dt()) % 0.7 < 0.5;
        self.block(if talking { voice } else { 0.0 }, tones);
        self.depth()
      })
      .collect()
  }

  /// Every game block over `seconds` passes bit for bit.
  fn assert_untouched(&mut self, seconds: f32, voice: f32, tones: &[(f64, f32)]) {
    for _ in 0..self.blocks(seconds) {
      let (dry, out) = self.block(voice, tones);
      assert_eq!(out, dry);
    }
  }

  /// Fully ducked (the bound): talk in a fight, more of the fight, then talk over it again.
  fn ducked(&mut self) {
    self.run(10.0, 0.0, FIGHT);
    self.run(12.0, VOICE, FIGHT);
    self.run(20.0, 0.0, FIGHT);
    self.run(2.0, VOICE, FIGHT);
    assert_eq!(self.depth(), T.max_depth_db);
  }

  fn depth(&self) -> f32 {
    self.game.gain.depth_db()
  }

  fn dt(&self) -> f32 {
    (self.frames as f64 / self.rate) as f32
  }

  fn blocks(&self, seconds: f32) -> usize {
    (f64::from(seconds) * self.rate / self.frames as f64).round() as usize
  }

  /// Stereo block of summed sines from the current sample on.
  fn signal(&self, tones: &[(f64, f32)]) -> Vec<f32> {
    (0..self.frames)
      .flat_map(|i| {
        let t = (self.phase + i) as f64 / self.rate;
        let s: f64 = tones.iter().map(|(f, a)| f64::from(*a) * (2.0 * std::f64::consts::PI * f * t).sin()).sum();
        [s as f32; 2]
      })
      .collect()
  }
}

// --- Nothing to do: the buffer is untouched ---

#[test]
fn loud_game_without_talk_is_bit_identical() {
  Rig::new().assert_untouched(3.0, 0.0, FIGHT);
}

#[test]
fn calm_game_with_talk_is_bit_identical() {
  let mut rig = Rig::new();
  rig.run(2.0, 0.0, &[(1500.0, CALM)]);
  rig.assert_untouched(3.0, VOICE, &[(1500.0, CALM)]);
}

#[test]
fn a_click_in_a_calm_game_does_not_duck() {
  let mut rig = Rig::new();
  for i in 0..rig.blocks(15.0) {
    let click = if i % 100 == 0 { 0.15 } else { 0.0 };
    rig.block(if i >= 500 { VOICE } else { 0.0 }, &[(1500.0, CALM), (1500.0, click)]);
    assert_eq!(rig.depth(), 0.0);
  }
}

#[test]
fn a_loud_chat_without_speech_does_not_duck() {
  let mut rig = Rig::new();
  rig.speech = false; // coughing, breathing, room noise
  rig.run(10.0, 0.0, FIGHT);
  rig.assert_untouched(3.0, LOUD_VOICE, FIGHT);
  assert_eq!(rig.table.slots[CHAT].speech_ms.load(Ordering::Relaxed), 0);
}

#[test]
fn a_stale_chat_slot_is_silence() {
  let mut rig = Rig::new();
  rig.run(3.0, 0.0, FIGHT);
  rig.table.slots[CHAT].speech_ms.store(0.01f32.to_bits(), Ordering::Relaxed);
  rig.table.slots[CHAT].last_audio_hns.store(rig.now - 1_000_000, Ordering::Relaxed);
  rig.assert_untouched(2.0, 0.0, FIGHT);
}

#[test]
fn a_muted_chat_or_a_quiet_game_does_not_duck() {
  let mut rig = Rig::new();
  rig.settings.chat_gain = 0.0;
  rig.run(10.0, 0.0, FIGHT);
  rig.assert_untouched(3.0, VOICE, FIGHT);

  let mut rig = Rig::new();
  rig.settings.game_gain = 0.05; // -26 dB: the voice is on top already
  rig.run(10.0, 0.0, FIGHT);
  rig.assert_untouched(3.0, VOICE, FIGHT);
}

#[test]
fn levels_sum_chat_and_game_slots_and_ignore_other_buses() {
  let rig = Rig::new();
  let now = 5_000_000_000;
  let slots = [(1, StreamBus::Chat, 0.1), (2, StreamBus::Chat, 0.2), (3, StreamBus::Game, 0.3)];
  let others = [(4, StreamBus::Game, 0.4), (5, StreamBus::None, 1.0), (6, StreamBus::Media, 1.0)];
  for (i, bus, ms) in slots.into_iter().chain(others) {
    let slot = &rig.table.slots[i];
    slot.bus.store(bus.to_u32(), Ordering::Relaxed);
    slot.speech_ms.store((ms as f32).to_bits(), Ordering::Relaxed);
    slot.last_audio_hns.store(now, Ordering::Relaxed);
  }
  let (voice, game) = levels(&rig.table, GAME, 0.5, now);
  assert!((voice - 0.3).abs() < 1e-6 && (game - 1.2).abs() < 1e-6, "{voice} {game}");
}

#[test]
fn a_game_plays_only_while_a_game_slot_had_audio_within_the_gap() {
  let rig = Rig::new();
  let now = 5_000_000_000;
  assert!(!game_playing(&rig.table, now), "a game slot that never played");
  rig.table.slots[CHAT].last_audio_hns.store(now, Ordering::Relaxed);
  assert!(!game_playing(&rig.table, now), "only the chat plays");
  rig.table.slots[GAME].last_audio_hns.store(now - GAP_HNS, Ordering::Relaxed);
  assert!(game_playing(&rig.table, now));
  assert!(!game_playing(&rig.table, now + 1), "the game stopped longer ago than the gap");
}

#[test]
fn default_strength_is_the_8_5_db_margin() {
  assert_eq!(DuckSettings::read(&ShmHeader::default()).strength * T.max_margin_db, 8.5);
}

// --- The cut: small first words, no steps, only the speech band ---

#[test]
fn a_single_word_dips_little_and_returns_exactly() {
  let mut rig = Rig::new();
  rig.run(10.0, 0.0, FIGHT);
  let word = rig.run(0.3, VOICE, FIGHT);
  let after = rig.run(T.hold_s + 1.0, 0.0, FIGHT);
  let peak = word.iter().chain(&after).fold(0.0f32, |m, d| m.max(*d));
  assert!(peak > 0.5 && peak <= 3.0, "dip {peak} dB");
  assert_eq!((rig.depth(), rig.game.last_gain), (0.0, 1.0));
  rig.assert_untouched(0.1, 0.0, FIGHT);
}

#[test]
fn a_sentence_reaches_the_depth_without_pumping_or_steps() {
  let mut rig = Rig::new();
  rig.run(10.0, 0.0, FIGHT);
  let depths = rig.sentence(3.0, VOICE, FIGHT);
  let max_db = T.max_depth_db;
  assert!(depths.iter().all(|d| *d <= max_db + 1e-4));
  assert!((depths.last().unwrap() - max_db).abs() < 0.01, "{depths:?}");
  for pair in depths.windows(2) {
    assert!(pair[1] >= pair[0] - 1e-3, "the depth went back up in a pause: {pair:?}");
    assert!(pair[1] - pair[0] <= T.fast_attack_db_per_s * 0.01 + 1e-4, "step {pair:?}");
  }
}

#[test]
fn the_gain_moves_smoothly_sample_by_sample() {
  let mut rig = Rig::new();
  rig.run(10.0, 0.0, FIGHT);
  let (mut dry, mut out) = (Vec::new(), Vec::new());
  for _ in 0..rig.blocks(1.0) {
    let (d, o) = rig.block(VOICE, FIGHT);
    dry.extend(d);
    out.extend(o);
  }
  // A steady tone at the band centre passes the band-pass unchanged: 1 - out / in is the cut.
  let cuts: Vec<(usize, f64)> = dry
    .iter()
    .zip(&out)
    .enumerate()
    .filter(|(_, (d, _))| d.abs() > 0.25)
    .map(|(n, (d, o))| (n / 2, 1.0 - f64::from(*o) / f64::from(*d)))
    .collect();
  assert!(cuts.last().unwrap().1 > 0.7, "it ducked");
  let per_frame = (1.0 - 10f64.powf(-f64::from(T.fast_attack_db_per_s) * 0.01 / 20.0)) / 480.0;
  for pair in cuts.windows(2) {
    let ((n0, c0), (n1, c1)) = (pair[0], pair[1]);
    assert!((c1 - c0).abs() <= per_frame * (n1 - n0) as f64 + 1e-5, "frame {n0} -> {n1}: {c0} -> {c1}");
  }
}

#[test]
fn the_whole_band_is_lowered_like_mix_modes_session_volume() {
  let mut rig = Rig::new();
  let tones = [(60.0, 0.3), (1500.0, 0.3)];
  rig.run(10.0, 0.0, &tones);
  rig.run(3.0, VOICE, &tones);
  let depth = rig.depth();
  assert!(depth > 12.0, "{depth}");
  let (first, (dry, out)) = (rig.phase, collect(&mut rig, 0.1, &tones));
  let change_db = |f| 20.0 * (amplitude(&out, first, f, 48_000.0) / amplitude(&dry, first, f, 48_000.0)).log10();
  assert!((change_db(60.0) + f64::from(depth)).abs() < 0.5, "60 Hz {}", change_db(60.0));
  assert!((change_db(1500.0) + f64::from(depth)).abs() < 0.5, "1.5 kHz {}", change_db(1500.0));
}

#[test]
fn rates_and_depth_do_not_depend_on_rate_or_block_size() {
  // The 400 Hz tone sits on the measuring band's flank: lowered like the rest at every rate.
  let tones = [(1500.0, LOUD), (400.0, 0.2)];
  let mut flank_at_48k = None;
  for (rate, frames) in [(48_000.0, 480), (44_100.0, 441), (44_100.0, 1024)] {
    let mut rig = Rig::at(rate, frames);
    let dt = rig.dt();
    rig.run(10.0, 0.0, &tones);
    let attack = rig.run(2.0, VOICE, &tones);
    assert_eq!(rig.depth(), T.max_depth_db, "{rate}/{frames}");
    // 0.2 s holds whole periods of both tones at both rates.
    let (first, (dry, out)) = (rig.phase, collect(&mut rig, 0.2, &tones));
    let len = 2 * (0.2 * rate) as usize;
    let change = |f| 20.0 * (amplitude(&out[..len], first, f, rate) / amplitude(&dry[..len], first, f, rate)).log10();
    assert!((change(1500.0) + T.max_depth_db as f64).abs() < 0.5, "{rate}/{frames}: {} dB", change(1500.0));
    let flank = *flank_at_48k.get_or_insert(change(400.0));
    assert!((change(400.0) - flank).abs() < 0.05, "{rate}/{frames}: 400 Hz {} vs {flank} dB", change(400.0));
    rig.run(T.hold_s + 0.1, 0.0, &tones); // the hold
    let release = rig.run(3.0, 0.0, &tones);
    let rates = |depths: &[f32], low: f32| -> Vec<f32> {
      depths.windows(2).filter(|p| p.iter().all(|d| *d > low && *d < 10.0)).map(|p| (p[1] - p[0]) / dt).collect()
    };
    let (up, down) = (rates(&attack, 4.0), rates(&release, 1.0));
    assert!(up.len() >= 2 && up.iter().all(|r| (r - T.fast_attack_db_per_s).abs() < 0.01), "{rate}/{frames}: {up:?}");
    assert!(down.len() >= 2 && down.iter().all(|r| (r + T.release_db_per_s).abs() < 0.01), "{rate}/{frames}: {down:?}");
  }
}

#[test]
fn too_many_channels_pass_and_a_bad_sample_does_not_poison_the_filter() {
  let mut rig = Rig::new();
  rig.ducked();
  let mut wide: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.37).sin() * 0.5).collect();
  let dry = wide.clone();
  rig.game.process(&mut wide, 10, DuckRole::Ducked, false, &rig.table, GAME, rig.now, &rig.settings);
  assert_eq!(wide, dry);

  let mut bad = rig.signal(FIGHT);
  (bad[10], bad[11]) = (f32::NAN, f32::INFINITY);
  rig.feed(VOICE, &mut bad);
  // The flushed band-pass needs about a millisecond to build up again: level from the next block on.
  for n in 0..10 {
    let (_, out) = rig.block(VOICE, FIGHT);
    let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(
      out.iter().all(|s| s.is_finite())
        && (n == 0 || (peak / (LOUD * 10f32.powf(-T.max_depth_db / 20.0)) - 1.0).abs() < 0.1),
      "peak {peak}"
    );
  }
}

// --- Releasing: off, channel lost, weaker strength ---

#[test]
fn turned_off_mid_duck_releases_smoothly_then_passes_through() {
  let mut rig = Rig::new();
  rig.ducked();
  rig.settings.enabled = false;
  let depths = rig.run(RELEASE_S, VOICE, FIGHT);
  assert_releases(&mut rig, &depths, 0.0);
  for slot in [GAME, CHAT] {
    assert_eq!(rig.table.slots[slot].speech_ms.load(Ordering::Relaxed), 0);
  }
}

#[test]
fn losing_the_game_channel_mid_duck_releases_smoothly() {
  let mut rig = Rig::new();
  rig.ducked();
  rig.role = DuckRole::Other;
  let depths = rig.run(RELEASE_S, VOICE, FIGHT);
  assert_releases(&mut rig, &depths, 0.0);
}

#[test]
fn a_lower_strength_mid_duck_comes_up_at_the_release_rate() {
  let mut rig = Rig::new();
  rig.ducked();
  rig.settings.strength = 0.0;
  // The level starts over and the talk raises it again at once, to what it needs now.
  let depths = rig.run(1.0, VOICE, FIGHT);
  let level = rig.game.status().level_db;
  assert!(level > 10.0 && level < T.max_depth_db - 1.0, "{level}");
  assert_releases(&mut rig, &depths, level);
}

// --- Across a run of the game ---

#[test]
fn two_firefights_get_the_same_depth() {
  let mut rig = Rig::new();
  rig.run(15.0, 0.0, FIGHT);
  let first = rig.sentence(3.0, LOUD_VOICE, FIGHT);
  rig.run(5.0, 0.0, &[(1500.0, CALM)]);
  // 6 dB quieter: the level stays while the game is heard.
  rig.run(3.0, 0.0, &[(1500.0, LOUD / 2.0)]);
  let second = rig.sentence(3.0, LOUD_VOICE, &[(1500.0, LOUD / 2.0)]);
  assert!(peak(&first) > 10.0 && peak(&first) < T.max_depth_db - 0.5, "{}", peak(&first));
  assert!((peak(&first) - peak(&second)).abs() < 1.0, "{} vs {}", peak(&first), peak(&second));
}

#[test]
fn calm_play_keeps_the_level_a_long_menu_starts_it_over() {
  let mut rig = Rig::new();
  rig.run(5.0, 0.0, FIGHT);
  let first = rig.sentence(5.0, LOUD_VOICE, FIGHT);
  let level = rig.game.status().level_db;
  rig.run(120.0, 0.0, &[(1500.0, CALM)]);
  assert!((rig.game.status().level_db - level).abs() < 0.25, "calm play is heard: only the decay");
  rig.run(80.0, 0.0, &[]); // a menu sending digital zeros
  assert_eq!(rig.game.status().level_db, 0.0);
  rig.run(1.0, 0.0, FIGHT);
  let second = rig.sentence(3.0, LOUD_VOICE, FIGHT);
  assert!((peak(&first) - peak(&second)).abs() < 1.0, "the same fight again: {} vs {}", peak(&first), peak(&second));
}

#[test]
fn bursts_of_gunfire_do_not_move_the_depth_within_a_sentence() {
  let mut rig = Rig::new();
  let mut depths = Vec::new();
  for i in 0..rig.blocks(19.0) {
    let game = if i % 20 < 10 { LOUD } else { 0.02 };
    let talking = i >= 1500 && i % 70 < 50;
    rig.block(if talking { LOUD_VOICE } else { 0.0 }, &[(1500.0, game)]);
    depths.push(rig.depth());
  }
  let (low, high) = depths[1650..].iter().fold((f32::MAX, 0.0f32), |(l, h), d| (l.min(*d), h.max(*d)));
  assert!(low > 6.0 && high - low < 1.0, "{low}..{high}");
}

#[test]
fn a_gap_in_the_stream_starts_the_level_over() {
  let mut rig = Rig::new();
  rig.run(15.0, 0.0, FIGHT);
  let before = rig.sentence(3.0, LOUD_VOICE, FIGHT);
  rig.now += 20_000_000; // 2 s without a block: menu, alt-tab
  rig.run(0.01, 0.0, FIGHT);
  assert_eq!((rig.depth(), rig.game.last_gain, rig.game.status().level_db), (0.0, 1.0, 0.0), "restarted");
  rig.run(0.5, 0.0, FIGHT);
  let after = rig.sentence(2.0, LOUD_VOICE, FIGHT);
  let (a, b) = (before.last().unwrap(), after.last().unwrap());
  assert!((a - b).abs() < 0.5, "{a} vs {b}");
}

// --- A session volume change, from the service ---

#[test]
fn a_new_volume_epoch_starts_the_level_over_and_a_reused_slot_reads_none() {
  let mut rig = Rig::new();
  rig.ducked();
  rig.run(0.1, 0.0, FIGHT); // the chat's last blocks still count as talk for 50 ms
  slot(&rig).duck_volume_epoch.store(1, Ordering::Relaxed);
  let depths = rig.run(RELEASE_S, 0.0, FIGHT);
  assert_eq!(rig.game.status().level_db, 0.0);
  assert_releases(&mut rig, &depths, 0.0);
  // The next talk learns again, at the new volume.
  rig.run(2.0, VOICE, FIGHT);
  assert_eq!(rig.depth(), T.max_depth_db);

  let info = StreamInfo { pid: 1, now_hns: 0, sample_rate: 48_000, channels: 2, mode: 0 };
  assert_eq!(rig.table.claim(&info).map(|c| c.0), Some(GAME));
  assert_eq!(slot(&rig).duck_volume_epoch.load(Ordering::Relaxed), 0);
}

// --- Tuning, telemetry, failure ---

#[test]
fn a_tuning_change_mid_duck_applies_next_block_through_the_slew_rates() {
  let mut rig = Rig::new();
  rig.ducked();
  let h = ShmHeader::default();
  let mut tuning = T;
  tuning.set("max_depth_db", 10.0);
  tuning.write_to(&h);
  rig.game.refresh_tuning(&h);
  let depths = rig.run(RELEASE_S, VOICE, FIGHT);
  assert!(depths[0] < T.max_depth_db, "next block");
  assert_releases(&mut rig, &depths, 10.0);
  // Another band: the depth carries on, the output stays sane.
  tuning.set("band_center_hz", 3000.0);
  tuning.write_to(&h);
  rig.game.refresh_tuning(&h);
  for _ in 0..50 {
    let (_, out) = rig.block(VOICE, FIGHT);
    assert!(rig.depth() == 10.0 && out.iter().all(|v| v.is_finite() && v.abs() < 1.0));
  }
}

#[test]
fn every_tuning_value_at_its_limits_keeps_output_and_depth_sane() {
  for &(name, _, min, max) in crate::ducking::tuning::PARAMS {
    for value in [min, max] {
      let mut tuning = T;
      assert!(tuning.set(name, value), "{name} {value}");
      let h = ShmHeader::default();
      tuning.write_to(&h);
      let mut rig = Rig::new();
      rig.game.refresh_tuning(&h);
      rig.chat.refresh_tuning(&h);
      for (voice, seconds) in [(0.0, 3.0), (VOICE, 4.0), (0.0, 1.0)] {
        for _ in 0..rig.blocks(seconds) {
          let (_, out) = rig.block(voice, FIGHT);
          let depth = rig.depth();
          assert!(out.iter().all(|v| v.is_finite()), "{name} {value}: output");
          assert!(depth.is_finite() && (0.0..=tuning.max_depth_db + 1e-4).contains(&depth), "{name} {value}: {depth}");
        }
      }
    }
  }
}

#[test]
fn debug_telemetry_shows_the_duckers_state_and_na_when_idle() {
  let mut rig = Rig::new();
  rig.ducked();
  let st = rig.game.status();
  let read = |slot: usize, index: usize| rig.table.slots[slot].duck_dbg[index].load(Ordering::Relaxed);
  let x10 = |v: f32| (v * 10.0).round() as i32;
  assert_eq!(read(GAME, dbg::DEPTH_DB), x10(st.depth_db));
  assert_eq!(read(GAME, dbg::LEVEL_DB), x10(st.level_db));
  assert_eq!(read(GAME, dbg::GAME_DB), x10(st.game_db));
  assert_eq!(read(GAME, dbg::SPURT_DB), x10(st.spurt_db.unwrap()));
  assert_eq!(read(GAME, dbg::FLAGS), dbg::FLAG_TALKING | dbg::FLAG_LOWERING);
  assert_eq!(read(GAME, dbg::LEVEL_RAISES), st.raises as i32);
  assert!(st.raises >= 1);
  assert!(read(CHAT, dbg::BAND_DB) != dbg::NA && read(CHAT, dbg::GAME_DB) == dbg::NA);
  rig.settings.enabled = false;
  rig.run(RELEASE_S, VOICE, FIGHT);
  assert!(rig.table.slots[GAME].duck_dbg.iter().all(|v| v.load(Ordering::Relaxed) == dbg::NA));
}

#[test]
fn a_failed_stream_reads_failed_and_stops_ducking_others() {
  let mut rig = Rig::new();
  rig.ducked();
  dbg::fail(&rig.table.slots[CHAT]);
  let chat = &rig.table.slots[CHAT];
  assert_eq!((chat.speech_ms.load(Ordering::Relaxed), chat.duck_permille.load(Ordering::Relaxed)), (0, 0));
  let dbg: Vec<i32> = chat.duck_dbg.iter().map(|v| v.load(Ordering::Relaxed)).collect();
  assert!(
    dbg.iter().enumerate().all(|(i, v)| *v == if i == dbg::FLAGS { dbg::FLAG_FAILED } else { dbg::NA }),
    "{dbg:?}"
  );
}

fn slot(rig: &Rig) -> &StreamSlot {
  &rig.table.slots[GAME]
}

/// After a release towards `target`: no step faster than the release rate, never back up, then
/// at `target`; at 0 the stream passes bit for bit and reports no reduction.
fn assert_releases(rig: &mut Rig, depths: &[f32], target: f32) {
  for pair in depths.windows(2) {
    assert!(pair[1] <= pair[0] && pair[0] - pair[1] <= T.release_db_per_s * rig.dt() + 1e-4, "{pair:?}");
  }
  assert!(depths[0] > target + 1.0 && (depths.last().unwrap() - target).abs() < 1e-4, "{depths:?}");
  if target == 0.0 {
    assert_eq!(rig.game.last_gain, 1.0);
    rig.assert_untouched(0.1, VOICE, FIGHT);
    assert_eq!(rig.table.slots[GAME].duck_permille.load(Ordering::Relaxed), 0);
  }
}

/// `seconds` of game blocks (input, output), the chat talking.
fn collect(rig: &mut Rig, seconds: f32, tones: &[(f64, f32)]) -> (Vec<f32>, Vec<f32>) {
  let (mut dry, mut out) = (Vec::new(), Vec::new());
  for _ in 0..(f64::from(seconds) * rig.rate / rig.frames as f64).ceil() as usize {
    let (d, o) = rig.block(VOICE, tones);
    dry.extend(d);
    out.extend(o);
  }
  (dry, out)
}

/// Amplitude of `freq` in the left channel (single-bin DFT; whole periods give no leakage).
fn amplitude(buf: &[f32], first: usize, freq: f64, rate: f64) -> f64 {
  let (mut re, mut im) = (0.0, 0.0);
  for (i, [left, _]) in buf.as_chunks::<2>().0.iter().enumerate() {
    let w = 2.0 * std::f64::consts::PI * freq * (first + i) as f64 / rate;
    re += f64::from(*left) * w.cos();
    im += f64::from(*left) * w.sin();
  }
  2.0 * re.hypot(im) / (buf.len() / 2) as f64
}

fn peak(depths: &[f32]) -> f32 {
  depths.iter().fold(0.0f32, |m, v| m.max(*v))
}
