//! Automatic ChatMix offline: runs a game and a chat recording through the stream APOs' ducking
//! path (a stream table, the chat stream with Silero VAD on a mono downmix and the gap reset of
//! `stream.rs::detect_speech`, the game stream, both unpaired until `--pair-delay` as real streams
//! are until the service pairs them, 10 ms blocks on a simulated clock), so tuning can be heard and
//! read without a game, a call or a new APO install. 48 kHz WAV only (other rates are refused).
//!
//! Not modelled: the chat stream's EQ, which the APO runs before the detector; more than one chat
//! or game stream; the engine leaving out buffers an app marks silent (the chat stream runs while
//! its file lasts); volumes other than the fixed gains given.
//!
//!   cargo run --release -p gamersequalizer-common --example duck_sim -- <game.wav> <chat.wav> <out_dir>
//!     [duck_tuning.json] [--strength 0.5] [--game-gain 1.0] [--chat-gain 1.0] [--pair-delay 3]
//!     [--seed <loud_db> <quiet_db>] [--mix]
//!
//! `--mix` runs the service's path instead (`MixDucker`, for games that bypass the stream effects):
//! the game stream is left alone, a 50 ms tick sees the output mix (game at the applied volume +
//! chat) and lowers the game's whole band from the next tick on.
//!
//! Writes `game_ducked.wav`, `mix_before.wav` and `mix_after.wav` (volumes applied) into
//! `out_dir`, and prints one line per talk spurt plus a summary. `duck_tuning.json` is the file
//! the service reads: a flat object of `DuckTuning` names to numbers. `--seed` takes the levels a
//! first run printed, to hear a second run of the same game, locked from its first sentence.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::Ordering;

use gamersequalizer_common::ducking::{
  DuckRole, DuckSettings, DuckTuning, GAP_HNS, MixDucker, StreamDucker, chat_levels, chat_sounding,
};
use gamersequalizer_common::shm::ShmHeader;
use gamersequalizer_common::streams::{StreamBus, StreamInfo, StreamTable};
use silero_vad::Vad48k;

const RATE: u32 = 48_000;
/// 10 ms, the audio engine's usual period.
const BLOCK: usize = 480;
const BLOCK_HNS: u64 = 100_000;
/// Seconds both streams run unpaired by default (the service pairs a stream seconds after it starts).
const PAIR_DELAY_S: f32 = 3.0;

/// Interleaved samples.
struct Audio {
  channels: usize,
  samples: Vec<f32>,
}

struct Options {
  game: PathBuf,
  chat: PathBuf,
  out: PathBuf,
  tuning: Option<PathBuf>,
  settings: DuckSettings,
  run: Run,
}

/// How a simulation runs: pairing delay, stored levels, the service's mix path.
#[derive(Default)]
struct Run {
  pair_delay_s: f32,
  seed: Option<(f32, f32)>,
  mix: bool,
}

/// One talk spurt as the game stream's ducker saw it.
struct Spurt {
  start_s: f32,
  talk_s: f32,
  level_db: f32,
  game_db: f32,
  ducked: bool,
  depth_db: f32,
}

struct Sim {
  game_ducked: Vec<f32>,
  spurts: Vec<Spurt>,
  talk_s: f32,
  ducked_talk_s: f32,
  loud_db: Option<f32>,
  quiet_db: Option<f32>,
  lock_db: Option<f32>,
  target_db: Option<f32>,
}

fn main() -> ExitCode {
  match run() {
    Ok(()) => ExitCode::SUCCESS,
    Err(err) => {
      eprintln!("duck_sim: {err}");
      ExitCode::FAILURE
    }
  }
}

fn run() -> Result<(), String> {
  let opts = parse_args(std::env::args().skip(1).collect())?;
  let mut tuning = DuckTuning::default();
  if let Some(path) = &opts.tuning {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    for ignored in apply_tuning(&mut tuning, &text)? {
      println!("tuning: ignored {ignored} (unknown name or out of range)");
    }
  }
  println!("tuning: {:?}", tuning.non_default());
  let (game, chat) = (read_wav(&opts.game)?, read_wav(&opts.chat)?);
  let mut vad = Vad48k::new().map_err(|e| format!("voice detector: {e}"))?;
  let mut last_prob = 0.0f32;
  let detector = |mono: &[f32], reset: bool| {
    if reset {
      vad.reset();
      last_prob = 0.0;
    }
    vad.process(mono, |p| last_prob = p);
    last_prob
  };
  let sim = simulate(&game, &chat, &tuning, &opts.settings, &opts.run, detector)?;

  std::fs::create_dir_all(&opts.out).map_err(|e| format!("{}: {e}", opts.out.display()))?;
  let s = &opts.settings;
  write_wav(&opts.out.join("game_ducked.wav"), game.channels, &sim.game_ducked)?;
  write_wav(&opts.out.join("mix_before.wav"), game.channels, &mix(&game, &game.samples, &chat, s))?;
  write_wav(&opts.out.join("mix_after.wav"), game.channels, &mix(&game, &sim.game_ducked, &chat, s))?;
  print_report(&sim);
  Ok(())
}

/// The core, shared with the self-check. `detector` gets each paired chat block as mono and
/// whether to start from fresh state first, and returns the speech probability of its last
/// completed chunk.
fn simulate(
  game: &Audio,
  chat: &Audio,
  tuning: &DuckTuning,
  settings: &DuckSettings,
  run: &Run,
  mut detector: impl FnMut(&[f32], bool) -> f32,
) -> Result<Sim, String> {
  let Run { pair_delay_s, seed, mix } = *run;
  let table = zeroed_table();
  let info = |channels: usize| StreamInfo { pid: 1, now_hns: 0, sample_rate: RATE, channels: channels as u32, mode: 0 };
  let game_slot = table.claim(&info(game.channels)).map(|c| c.0).ok_or("no stream slot")?;
  let chat_slot = table.claim(&info(chat.channels)).map(|c| c.0).ok_or("no stream slot")?;
  let slot = |index: usize| table.slots.get(index).ok_or("no stream slot");
  slot(game_slot)?.bus.store(StreamBus::Game.to_u32(), Ordering::Relaxed);
  slot(chat_slot)?.bus.store(StreamBus::Chat.to_u32(), Ordering::Relaxed);
  if let Some((loud, quiet)) = seed {
    slot(game_slot)?.duck_saved_loud_db_x10.store((loud * 10.0).round() as i32, Ordering::Relaxed);
    slot(game_slot)?.duck_saved_quiet_db_x10.store((quiet * 10.0).round() as i32, Ordering::Relaxed);
  }
  let header = Box::new(ShmHeader::default());
  tuning.write_to(&header);
  let (mut game_ducker, mut chat_ducker) = (StreamDucker::new(RATE as f32), StreamDucker::new(RATE as f32));
  game_ducker.refresh_tuning(&header);
  chat_ducker.refresh_tuning(&header);
  // The service's path (`--mix`): its 50 ms tick, the mix it heard and the depth it applies.
  let (mut mixer, mut tick_mix, mut mix_depth) = (MixDucker::new(RATE as f32), Vec::new(), 0.0f32);
  if let Some((loud, quiet)) = seed.filter(|_| mix) {
    mixer.seed(Some(loud), Some(quiet));
  }

  let frames = game.samples.len() / game.channels.max(1);
  let chat_frames = chat.samples.len() / chat.channels.max(1);
  let mut vad_hns: Option<u64> = None;
  let (mut out, mut mono, mut chat_block) = (Vec::with_capacity(game.samples.len()), Vec::new(), Vec::new());
  let mut sim = Sim {
    game_ducked: Vec::new(),
    spurts: Vec::new(),
    talk_s: 0.0,
    ducked_talk_s: 0.0,
    loud_db: None,
    quiet_db: None,
    lock_db: None,
    target_db: None,
  };
  let mut spurt: Option<(Spurt, f32, usize)> = None; // the spurt, its game level sum and blocks
  let mut now = 1_000_000_000u64;
  for first in (0..frames).step_by(BLOCK) {
    now += BLOCK_HNS;
    let count = BLOCK.min(frames - first);
    let t = first as f32 / RATE as f32;
    let paired = t >= pair_delay_s;
    // The chat stream runs while its file lasts, every block recorded as the APO does.
    if first < chat_frames {
      chat_block.clear();
      chat_block.extend(
        (0..count * chat.channels).map(|i| chat.samples.get(first * chat.channels + i).copied().unwrap_or(0.0)),
      );
      let peak = chat_block.iter().fold(0.0f32, |m, s| m.max(s.abs()));
      slot(chat_slot)?.record(now, peak, count as u32);
      let role = if paired { DuckRole::Voice } else { DuckRole::Other };
      // As `stream.rs::detect_speech`: only a paired chat stream with the feature on feeds the
      // detector, and a gap restarts it.
      let speech = role == DuckRole::Voice && settings.enabled && {
        mono.clear();
        mono.extend(chat_block.chunks_exact(chat.channels).map(|f| f.iter().sum::<f32>() / f.len() as f32));
        let reset = vad_hns.is_none_or(|t| now.saturating_sub(t) > GAP_HNS);
        vad_hns = Some(now);
        detector(&mono, reset) >= tuning.speech_prob
      };
      chat_ducker.process(&mut chat_block, chat.channels, role, speech, &table, chat_slot, now, settings);
    }
    let start = first * game.channels;
    let mut block = game.samples.get(start..start + count * game.channels).ok_or("short game file")?.to_vec();
    let role = if paired && !mix { DuckRole::Ducked } else { DuckRole::Other };
    game_ducker.process(&mut block, game.channels, role, false, &table, game_slot, now, settings);
    if mix {
      let gain = 10f32.powf(-mix_depth / 20.0);
      block.iter_mut().for_each(|s| *s *= gain);
      let chat_at = |i: usize| chat.samples.get((first + i) * chat.channels).copied().unwrap_or(0.0);
      for (i, frame) in block.chunks_exact(game.channels).enumerate() {
        let sample =
          frame.iter().sum::<f32>() / frame.len() as f32 * settings.game_gain + chat_at(i) * settings.chat_gain;
        tick_mix.extend([sample; 2]);
      }
      if (first / BLOCK) % 5 == 4 {
        // As the service: the chat's speech level, and whether its envelope (recorded above per
        // 10 ms block) sounded during the tick, with a small margin.
        let (speech_ms, _) = chat_levels(&table, now);
        let sounding = chat_sounding(&table, now, 0.06);
        let s = DuckSettings { enabled: settings.enabled && paired, ..*settings };
        mix_depth = mixer.process(&tick_mix, 0.05, speech_ms, sounding, &s, tuning);
        tick_mix.clear();
      }
    }
    out.extend_from_slice(&block);

    let st = if mix { mixer.status() } else { game_ducker.status() };
    if st.talking {
      sim.talk_s += count as f32 / RATE as f32;
      if st.latched {
        sim.ducked_talk_s += count as f32 / RATE as f32;
      }
    }
    match (&mut spurt, st.spurt_talk_s > 0.0) {
      (None, true) => {
        let s = Spurt { start_s: t, talk_s: 0.0, level_db: 0.0, game_db: 0.0, ducked: false, depth_db: 0.0 };
        spurt = Some((s, st.game_db, 1));
      }
      (Some((s, game_sum, blocks)), true) => {
        (s.talk_s, s.level_db) = (st.spurt_talk_s, st.spurt_db.unwrap_or(0.0));
        s.ducked |= st.latched;
        s.depth_db = s.depth_db.max(st.depth_db);
        (*game_sum, *blocks) = (*game_sum + st.game_db, *blocks + 1);
      }
      (Some(_), false) => {
        if let Some((mut s, game_sum, blocks)) = spurt.take() {
          s.game_db = game_sum / blocks as f32;
          sim.spurts.push(s);
        }
      }
      (None, false) => {}
    }
    (sim.loud_db, sim.quiet_db, sim.lock_db, sim.target_db) = (st.loud_db, st.quiet_db, st.lock_db, st.target_db);
  }
  sim.game_ducked = out;
  Ok(sim)
}

fn print_report(sim: &Sim) {
  println!("  start   talk    spurt   game    ducked  depth");
  for s in &sim.spurts {
    println!(
      "{:7.2} s {:5.2} s {:6.1} {:6.1} dB  {:6}  {:4.1} dB",
      s.start_s,
      s.talk_s,
      s.level_db,
      s.game_db,
      if s.ducked { "yes" } else { "no" },
      s.depth_db
    );
  }
  let db = |v: Option<f32>| v.map_or("-".to_string(), |v| format!("{v:.1} dB"));
  println!(
    "learned loud level {}, quiet talker {}, locked difference {}, depth when ducking {}, talk ducked {:.0} %",
    db(sim.loud_db),
    db(sim.quiet_db),
    db(sim.lock_db),
    db(sim.target_db),
    100.0 * sim.ducked_talk_s / sim.talk_s.max(1e-6)
  );
  if let (Some(loud), Some(quiet)) = (sim.loud_db, sim.quiet_db) {
    println!("a second run of this game: --seed {loud:.1} {quiet:.1}");
  }
}

/// Applies a flat JSON object of tuning names to numbers; returns what was ignored.
fn apply_tuning(tuning: &mut DuckTuning, text: &str) -> Result<Vec<String>, String> {
  let value: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("tuning file: {e}"))?;
  let object = value.as_object().ok_or("tuning file: not a JSON object")?;
  Ok(
    object
      .iter()
      .filter(|(name, value)| !value.as_f64().is_some_and(|v| tuning.set(name, v as f32)))
      .map(|(name, _)| name.clone())
      .collect(),
  )
}

/// The listener's mix: the game and the chat (mono, on every game channel) at their volumes.
fn mix(game: &Audio, game_samples: &[f32], chat: &Audio, s: &DuckSettings) -> Vec<f32> {
  let chat_mono = chat.samples.chunks_exact(chat.channels.max(1)).map(|f| f.iter().sum::<f32>() / f.len() as f32);
  game_samples
    .chunks_exact(game.channels.max(1))
    .zip(chat_mono.chain(std::iter::repeat(0.0)))
    .flat_map(|(frame, voice)| frame.iter().map(move |g| g * s.game_gain + voice * s.chat_gain))
    .collect()
}

fn parse_args(args: Vec<String>) -> Result<Options, String> {
  let usage = "usage: duck_sim <game.wav> <chat.wav> <out_dir> [duck_tuning.json] [--strength 0.5] \
               [--game-gain 1.0] [--chat-gain 1.0] [--pair-delay 3] [--seed <loud_db> <quiet_db>] [--mix]";
  let mut positional = Vec::new();
  let mut settings = DuckSettings { enabled: true, strength: 0.5, game_gain: 1.0, chat_gain: 1.0 };
  let mut run = Run { pair_delay_s: PAIR_DELAY_S, ..Run::default() };
  let mut args = args.into_iter();
  let number = |args: &mut std::vec::IntoIter<String>, flag: &str| -> Result<f32, String> {
    args.next().and_then(|v| v.parse().ok()).ok_or(format!("{flag} needs a number; {usage}"))
  };
  while let Some(arg) = args.next() {
    match arg.as_str() {
      "--strength" => settings.strength = number(&mut args, "--strength")?.clamp(0.0, 1.0),
      "--game-gain" => settings.game_gain = number(&mut args, "--game-gain")?.max(0.0),
      "--chat-gain" => settings.chat_gain = number(&mut args, "--chat-gain")?.max(0.0),
      "--pair-delay" => run.pair_delay_s = number(&mut args, "--pair-delay")?.max(0.0),
      "--seed" => run.seed = Some((number(&mut args, "--seed")?, number(&mut args, "--seed")?)),
      "--mix" => run.mix = true,
      _ if arg.starts_with("--") => return Err(format!("unknown option {arg}; {usage}")),
      _ => positional.push(PathBuf::from(arg)),
    }
  }
  let mut positional = positional.into_iter();
  match (positional.next(), positional.next(), positional.next(), positional.next(), positional.next()) {
    (Some(game), Some(chat), Some(out), tuning, None) => Ok(Options { game, chat, out, tuning, settings, run }),
    _ => Err(usage.to_string()),
  }
}

fn read_wav(path: &Path) -> Result<Audio, String> {
  let mut reader = hound::WavReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
  let spec = reader.spec();
  if spec.sample_rate != RATE {
    return Err(format!("{}: {} Hz; duck_sim reads 48 kHz only (resample it first)", path.display(), spec.sample_rate));
  }
  let samples: Result<Vec<f32>, _> = match spec.sample_format {
    hound::SampleFormat::Float => reader.samples::<f32>().collect(),
    hound::SampleFormat::Int => {
      let scale = 1.0 / (1u64 << (spec.bits_per_sample.max(1) - 1)) as f32;
      reader.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect()
    }
  };
  let samples = samples.map_err(|e| format!("{}: {e}", path.display()))?;
  Ok(Audio { channels: usize::from(spec.channels.max(1)), samples })
}

fn write_wav(path: &Path, channels: usize, samples: &[f32]) -> Result<(), String> {
  let spec = hound::WavSpec {
    channels: channels as u16,
    sample_rate: RATE,
    bits_per_sample: 32,
    sample_format: hound::SampleFormat::Float,
  };
  let err = |e: hound::Error| format!("{}: {e}", path.display());
  let mut writer = hound::WavWriter::create(path, spec).map_err(err)?;
  for sample in samples {
    writer.write_sample(*sample).map_err(err)?;
  }
  writer.finalize().map_err(err)
}

/// A fresh stream table, as the APO's mapping starts.
fn zeroed_table() -> Box<StreamTable> {
  let mut mem: Box<std::mem::MaybeUninit<StreamTable>> = Box::new_uninit();
  // SAFETY: all-zero bytes are a valid StreamTable (atomics only), like a fresh mapping; the
  // pointer is the box's own allocation.
  unsafe {
    std::ptr::write_bytes(mem.as_mut_ptr(), 0, 1);
    mem.assume_init()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 10 s of a 1.5 kHz game tone and a 1 kHz voice talking 2-5 s and 6-9 s; the detector here
  /// is the voice's level (a tone is no speech to Silero).
  fn run(game_amplitude: f32) -> (Audio, Sim) {
    let tone = |f: f32, a: f32, t: f32| a * (2.0 * std::f32::consts::PI * f * t).sin();
    let game = Audio {
      channels: 2,
      samples: (0..10 * RATE as usize)
        .flat_map(|i| [tone(1500.0, game_amplitude, i as f32 / RATE as f32); 2])
        .collect(),
    };
    let talking = |t: f32| (2.0..5.0).contains(&t) || (6.0..9.0).contains(&t);
    let chat = Audio {
      channels: 1,
      samples: (0..10 * RATE as usize)
        .map(|i| i as f32 / RATE as f32)
        .map(|t| if talking(t) { tone(1000.0, 0.1, t) } else { 0.0 })
        .collect(),
    };
    let settings = DuckSettings { enabled: true, strength: 0.5, game_gain: 1.0, chat_gain: 1.0 };
    let speech = |mono: &[f32], _reset: bool| if mono.iter().any(|s| s.abs() > 0.01) { 1.0 } else { 0.0 };
    let run = Run { pair_delay_s: PAIR_DELAY_S, ..Run::default() };
    let sim = simulate(&game, &chat, &DuckTuning::default(), &settings, &run, speech).unwrap();
    (game, sim)
  }

  #[test]
  fn ducks_speech_over_a_loud_game_and_not_over_a_calm_one() {
    let (game, loud) = run(0.5);
    assert_eq!(loud.spurts.len(), 2);
    assert!(loud.spurts.iter().all(|s| s.ducked && s.depth_db > 5.0), "{}", loud.spurts[0].depth_db);
    // Unpaired for the first 3 s: untouched even though the voice starts at 2 s.
    let paired = 2 * (PAIR_DELAY_S * RATE as f32) as usize;
    assert_eq!(loud.game_ducked[..paired], game.samples[..paired]);
    assert!(loud.spurts[0].start_s >= PAIR_DELAY_S);
    assert_ne!(loud.game_ducked, game.samples);
    let (game, calm) = run(0.005);
    assert!(calm.spurts.iter().all(|s| !s.ducked));
    assert_eq!(calm.game_ducked, game.samples);
  }
}
