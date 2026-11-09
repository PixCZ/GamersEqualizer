//! Spec 6.3 / 6.4 reports: old nnnoiseless (2017 network) vs this port's Full and Little models.
//! `cargo test --manifest-path crates/vendor/rnnoise2/Cargo.toml --release --test report -- --ignored --nocapture`

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, reason = "test reports")]

use std::time::Instant;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../test_data/denoise");
const FRAME: usize = 480;
/// RNNoise works in the i16 range; the metric helpers below expect +-1.
const I16_SCALE: f32 = 32768.0;
const VOICE: f32 = 0.5;
const SPEECH_DB: f32 = -20.0;
/// Soft syllables (spec 6.3): speech frames quieter than this, see `Report::soft_syllable_loss`.
const SOFT_SYLLABLE_DBFS: f32 = -35.0;

#[derive(Clone, Copy, Debug)]
enum Kind {
  Nnnoiseless,
  Full,
  Little,
}

impl Kind {
  const ALL: [Kind; 3] = [Kind::Nnnoiseless, Kind::Full, Kind::Little];

  fn latency(self) -> usize {
    match self {
      Kind::Nnnoiseless => FRAME,
      Kind::Full | Kind::Little => rnnoise2::LATENCY_FRAMES * rnnoise2::FRAME_SIZE,
    }
  }

  fn name(self) -> &'static str {
    match self {
      Kind::Nnnoiseless => "nnnoiseless 0.5",
      Kind::Full => "rnnoise2 Full",
      Kind::Little => "rnnoise2 Little",
    }
  }
}

/// Runs `input` (+-1 scale) through a fresh denoiser in 480-sample frames. Returns the output
/// (+-1, same length, trailing partial frame zero) and the voice probability per input frame.
fn run(kind: Kind, input: &[f32]) -> (Vec<f32>, Vec<f32>) {
  enum State {
    Old(Box<nnnoiseless::DenoiseState<'static>>),
    New(Box<rnnoise2::DenoiseState>),
  }
  let mut state = match kind {
    Kind::Nnnoiseless => State::Old(nnnoiseless::DenoiseState::new()),
    Kind::Full => State::New(rnnoise2::DenoiseState::with_model(rnnoise2::Model::Full).unwrap()),
    Kind::Little => State::New(rnnoise2::DenoiseState::with_model(rnnoise2::Model::Little).unwrap()),
  };
  let mut out = vec![0.0f32; input.len()];
  let mut vad = Vec::new();
  let (mut frame_in, mut frame_out) = ([0.0f32; FRAME], [0.0f32; FRAME]);
  for (i, o) in input.as_chunks::<FRAME>().0.iter().zip(out.as_chunks_mut::<FRAME>().0) {
    frame_in.iter_mut().zip(i).for_each(|(d, s)| *d = s * I16_SCALE);
    vad.push(match &mut state {
      State::Old(s) => s.process_frame(&mut frame_out, &frame_in),
      State::New(s) => s.process_frame(&mut frame_out, &frame_in),
    });
    o.iter_mut().zip(&frame_out).for_each(|(d, s)| *d = s / I16_SCALE);
  }
  (out, vad)
}

#[test]
#[ignore = "report, slow"]
fn rnnoise2_report() {
  println!("Speech at {SPEECH_DB} dBFS + keyboard/mouse clips (shared fixtures); first 50 frames skipped.\n");
  println!("| Mix | Model | Pause supp. dB | Far pause supp. dB | Loud lost % | Soft lost % | Mean loss dB | Lost >6 dB % | Soft syll. lost % | VAD on speech % |");
  println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
  for snr in [0.0, -10.0] {
    let (clean, noise, mixed) = mix("speech_clean.wav", &KEYBOARD_CLIPS, SPEECH_DB, snr);
    for kind in Kind::ALL {
      let (out, vad) = run(kind, &mixed);
      let q = measure(&clean, &mixed, &out, kind.latency());
      let c = click_loss(&clean, &noise, &out, kind.latency());
      let soft_syllables = soft_syllable_loss(&clean, &out, kind.latency());
      let vad_speech = vad_on_speech(&clean, &vad, kind.latency() / FRAME - 1);
      println!(
        "| {snr} dB SNR | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.2} | {:.1} | {:.1} | {:.1} |",
        kind.name(),
        q.noise_suppression_db,
        q.far_suppression_db,
        100.0 * q.dropouts,
        100.0 * q.soft_dropouts,
        c.mean_loss_db,
        100.0 * c.lost6,
        100.0 * soft_syllables,
        100.0 * vad_speech,
      );
    }
  }
  println!("\nKeys only at {SPEECH_DB} dBFS (no speech); first 50 frames skipped.\n");
  println!("| Model | Suppression dB | VAD >= 0.5 % |");
  println!("| --- | --- | --- |");
  let keys = keyboard_only(SPEECH_DB);
  for kind in Kind::ALL {
    let (out, vad) = run(kind, &keys);
    let skip = 50 * FRAME;
    let e_in: f32 = keys[skip..keys.len() - kind.latency()].iter().map(|x| x * x).sum();
    let e_out: f32 = out[skip + kind.latency()..].iter().map(|x| x * x).sum();
    let voiced =
      vad.iter().skip(50).filter(|&&v| v >= VOICE).count() as f32 / vad.len().saturating_sub(50).max(1) as f32;
    println!(
      "| {} | {:.1} | {:.1} |",
      kind.name(),
      10.0 * (e_in.max(1e-12) / e_out.max(1e-12)).log10(),
      100.0 * voiced
    );
  }
}

#[test]
#[ignore = "benchmark, run with --release"]
fn rnnoise2_bench() {
  const FRAMES: usize = 1000; // 10 s
  let (_, _, mixed) = mix("speech_clean.wav", &KEYBOARD_CLIPS, SPEECH_DB, 0.0);
  let input = &mixed[..FRAMES * FRAME];
  println!("| Model | ns / frame | % of real time (10 ms) | `voice_probability` ns / frame |");
  println!("| --- | --- | --- | --- |");
  for kind in Kind::ALL {
    let start = Instant::now();
    let (out, _) = run(kind, input);
    let ns = start.elapsed().as_nanos() as f64 / FRAMES as f64;
    std::hint::black_box(out);
    let model = match kind {
      Kind::Nnnoiseless => None,
      Kind::Full => Some(rnnoise2::Model::Full),
      Kind::Little => Some(rnnoise2::Model::Little),
    };
    let vad_ns = model.map_or("-".to_string(), |model| {
      let mut state = rnnoise2::DenoiseState::with_model(model).unwrap();
      let frames: Vec<f32> = input.iter().map(|s| s * I16_SCALE).collect();
      let start = Instant::now();
      for frame in frames.as_chunks::<FRAME>().0 {
        std::hint::black_box(state.voice_probability(frame));
      }
      format!("{:.0}", start.elapsed().as_nanos() as f64 / FRAMES as f64)
    });
    println!("| {} | {:.0} | {:.2} | {vad_ns} |", kind.name(), ns, ns / 10_000_000.0 * 100.0);
  }
}

/// Voice-detector operating points (the `in_net` / `out_net` roles): share of frames whose voice
/// probability reaches each threshold, for speech under keys (loud and quiet frames separately)
/// and for keys alone (false alarms). A gate that must never cut speech wants the top rows high
/// and the last column merely tolerable.
#[test]
#[ignore = "report, slow"]
fn vad_sweep() {
  println!("Speech frames: clean frame RMS >= clean RMS - 15 dB; quiet speech: clean RMS - 40 .. - 15 dB (syllable onsets/tails); keys only at {SPEECH_DB} dBFS. First 50 frames skipped.\n");
  println!("| Threshold | Model | Speech 0 dB % | Quiet 0 dB % | Speech -10 dB % | Quiet -10 dB % | Keys only % |");
  println!("| --- | --- | --- | --- | --- | --- | --- |");
  let mixes: Vec<_> =
    [0.0, -10.0].iter().map(|&snr| mix("speech_clean.wav", &KEYBOARD_CLIPS, SPEECH_DB, snr)).collect();
  let keys = keyboard_only(SPEECH_DB);
  for kind in Kind::ALL {
    let shift = kind.latency() / FRAME - 1;
    let vads: Vec<Vec<f32>> = mixes.iter().map(|(_, _, mixed)| run(kind, mixed).1).collect();
    let keys_vad = run(kind, &keys).1;
    for threshold in [0.02f32, 0.05, 0.1, 0.2, 0.3, 0.5, 0.7, 0.9] {
      let mut cells = Vec::new();
      for ((clean, _, _), vad) in mixes.iter().zip(&vads) {
        let clean_db = db(rms(clean));
        let (mut n_speech, mut hit_speech, mut n_quiet, mut hit_quiet) = (0usize, 0usize, 0usize, 0usize);
        for (c, &v) in clean.as_chunks::<FRAME>().0.iter().zip(vad.get(shift..).unwrap_or_default()).skip(50) {
          let c_db = db(rms(c));
          if c_db >= clean_db - 15.0 {
            n_speech += 1;
            hit_speech += (v >= threshold) as usize;
          } else if c_db >= clean_db - 40.0 {
            n_quiet += 1;
            hit_quiet += (v >= threshold) as usize;
          }
        }
        cells.push(format!(
          "{:.1} | {:.1}",
          100.0 * hit_speech as f32 / n_speech.max(1) as f32,
          100.0 * hit_quiet as f32 / n_quiet.max(1) as f32
        ));
      }
      let false_alarms = keys_vad.iter().skip(50).filter(|&&v| v >= threshold).count() as f32
        / keys_vad.len().saturating_sub(50).max(1) as f32;
      println!("| {threshold} | {} | {} | {:.1} |", kind.name(), cells.join(" | "), 100.0 * false_alarms);
    }
  }
}

/// Scratch dir shared with the upstream C demo (`.claude/tmp/rnnoise2/build/demo/rnnoise_demo_blob`).
const C_COMPARE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../.claude/tmp/rnnoise2/c_compare");

/// One 6.3 input: `(name, clean, noise, mixed)`; keys only has no clean/noise.
type ReportInput = (&'static str, Vec<f32>, Vec<f32>, Vec<f32>);

fn report_inputs() -> Vec<ReportInput> {
  let mut inputs = Vec::new();
  for (name, snr) in [("mix0", 0.0), ("mix-10", -10.0)] {
    let (clean, noise, mixed) = mix("speech_clean.wav", &KEYBOARD_CLIPS, SPEECH_DB, snr);
    inputs.push((name, clean, noise, mixed));
  }
  inputs.push(("keys", Vec::new(), Vec::new(), keyboard_only(SPEECH_DB)));
  inputs
}

fn write_raw_i16(path: &str, x: &[f32]) {
  let bytes: Vec<u8> =
    x.iter().flat_map(|s| ((s * I16_SCALE).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes()).collect();
  std::fs::write(path, bytes).expect(path);
}

fn read_raw_i16(path: &str) -> Vec<f32> {
  let bytes = std::fs::read(path).expect(path);
  bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b) as f32 / I16_SCALE).collect()
}

/// Step 1 of the C cross-check: writes the 6.3 inputs as raw i16 for `rnnoise_demo`.
/// Then, in `.claude/tmp/rnnoise2/build/{full,little}` (where `weights_blob.bin` lives):
/// `../demo/rnnoise_demo_blob ../../c_compare/in_<name>.raw ../../c_compare/c_<name>_<model>.raw`.
#[test]
#[ignore = "writes scratch files for the C demo"]
fn dump_report_mixes() {
  std::fs::create_dir_all(C_COMPARE).unwrap();
  for (name, _, _, mixed) in report_inputs() {
    let path = format!("{C_COMPARE}/in_{name}.raw");
    write_raw_i16(&path, &mixed);
    println!("{path}: {} samples", mixed.len());
  }
}

/// Step 2: the 6.3 metrics for the upstream C output and for this port on the same (i16-rounded)
/// inputs, plus the sample-wise difference between the two. Answers "is the port or the model worse".
#[test]
#[ignore = "needs the C demo outputs from dump_report_mixes"]
fn report_vs_upstream_c() {
  println!("| Input | Model | Output | Pause supp. dB | Far pause supp. dB | Loud lost % | Soft lost % | Mean loss dB | Lost >6 dB % | Soft syll. lost % | Port vs C max / RMS [LSB] |");
  println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
  for (name, clean, noise, _) in report_inputs() {
    let input = read_raw_i16(&format!("{C_COMPARE}/in_{name}.raw"));
    for (kind, model) in [(Kind::Full, "full"), (Kind::Little, "little")] {
      let c_file = read_raw_i16(&format!("{C_COMPARE}/c_{name}_{model}.raw"));
      // The demo drops the first output frame: put it back so the file aligns with `run`'s output.
      let mut c_out = vec![0.0f32; FRAME];
      c_out.extend_from_slice(&c_file);
      c_out.resize(input.len(), 0.0);
      let (ours, _) = run(kind, &input);
      // Quantize like the demo's `(short)` cast so both outputs share the i16 floor (a frame below
      // 1 LSB is digital zero in both). Rust saturates where C wraps; wrapped samples are counted.
      let ours: Vec<f32> = ours.iter().map(|s| (s * I16_SCALE) as i16 as f32 / I16_SCALE).collect();
      let (mut max, mut sq, mut n, mut wrapped) = (0.0f32, 0.0f64, 0usize, 0usize);
      for (a, b) in ours.iter().zip(&c_out).skip(FRAME).take(c_file.len()) {
        let d = ((a - b) * I16_SCALE).abs();
        if d > 30000.0 {
          wrapped += 1;
          continue;
        }
        max = max.max(d);
        sq += (d as f64) * (d as f64);
        n += 1;
      }
      let mut diff = format!("{max:.0} / {:.3}", (sq / n.max(1) as f64).sqrt());
      if wrapped > 0 {
        diff += &format!(" ({wrapped} wrapped in C)");
      }
      let latency = kind.latency();
      if clean.is_empty() {
        let skip = 50 * FRAME;
        let e_in: f32 = input[skip..input.len() - latency].iter().map(|x| x * x).sum();
        let supp = |out: &[f32]| {
          let e_out: f32 = out[skip + latency..].iter().map(|x| x * x).sum();
          10.0 * (e_in.max(1e-12) / e_out.max(1e-12)).log10()
        };
        println!("| {name} | {model} | upstream C | {:.1} | | | | | | | |", supp(&c_out));
        println!("| {name} | {model} | rnnoise2 | {:.1} | | | | | | | {diff} |", supp(&ours));
      } else {
        for (label, out, d) in [("upstream C", &c_out, ""), ("rnnoise2", &ours, diff.as_str())] {
          let q = measure(&clean, &input, out, latency);
          let c = click_loss(&clean, &noise, out, latency);
          let soft = soft_syllable_loss(&clean, out, latency);
          println!(
            "| {name} | {model} | {label} | {:.1} | {:.1} | {:.1} | {:.1} | {:.2} | {:.1} | {:.1} | {d} |",
            q.noise_suppression_db,
            q.far_suppression_db,
            100.0 * q.dropouts,
            100.0 * q.soft_dropouts,
            c.mean_loss_db,
            100.0 * c.lost6,
            100.0 * soft,
          );
        }
      }
    }
  }
}

/// Spec 6.3 "soft syllables": share of speech frames below -35 dBFS that lose more than 10 dB.
/// `measure` counts speech down to clean RMS - 15 dB (-35 dBFS at -20 dBFS speech), so these
/// are the frames between that and its pause level (clean RMS - 40 dB): onsets and tails.
fn soft_syllable_loss(clean: &[f32], out: &[f32], latency: usize) -> f32 {
  let clean_db = db(rms(clean));
  let (mut n, mut lost) = (0usize, 0usize);
  for (c, o) in clean.as_chunks::<FRAME>().0.iter().zip(out[latency..].as_chunks::<FRAME>().0).skip(50) {
    let c_db = db(rms(c));
    if c_db < SOFT_SYLLABLE_DBFS && c_db >= clean_db - 40.0 {
      n += 1;
      lost += (c_db - db(rms(o)) > 10.0) as usize;
    }
  }
  lost as f32 / n.max(1) as f32
}

/// Share of speech frames (clean frame RMS >= clean RMS - 15 dB, `measure`'s loud + soft) with
/// voice probability >= 0.5. `vad[t]` describes the frame the model's gains target: the current
/// analysis frame for nnnoiseless, frame t-1 for rnnoise2 (`shift` = latency in frames - 1).
fn vad_on_speech(clean: &[f32], vad: &[f32], shift: usize) -> f32 {
  let clean_db = db(rms(clean));
  let (mut n, mut voiced) = (0usize, 0usize);
  for (c, &v) in clean.as_chunks::<FRAME>().0.iter().zip(vad.get(shift..).unwrap_or_default()).skip(50) {
    if db(rms(c)) >= clean_db - 15.0 {
      n += 1;
      voiced += (v >= VOICE) as usize;
    }
  }
  voiced as f32 / n.max(1) as f32
}

// ---------------------------------------------------------------------------------------------
// Copied from `crates/gamersequalizer-mic-vst/src/ai_denoiser.rs` (tests module), unchanged except:
// `mix` has no low-cut option and also returns the noise; `click_loss` is the measuring half
// of `run_clicks`; `FIXTURES` points at the same directory from this crate.
// ---------------------------------------------------------------------------------------------

const KEYBOARD_CLIPS: [&str; 6] = [
  "1-137-A-32.wav",
  "1-53501-A-32.wav",
  "1-118206-A-31.wav",
  "1-61212-A-32.wav",
  "1-18655-A-31.wav",
  "1-79711-A-32.wav",
];

#[derive(Debug)]
struct Quality {
  /// Share of loud speech frames the output lost (more than 15 dB below the clean speech).
  dropouts: f32,
  /// Same as `dropouts` for soft speech (syllable onsets/tails, up to 15 dB below average).
  soft_dropouts: f32,
  /// How much quieter the output is than the input in speech pauses (keyboard only), dB.
  noise_suppression_db: f32,
  /// Same, only pauses at least `FAR_PAUSE_FRAMES` from speech.
  far_suppression_db: f32,
}

/// 150 ms: the rescue's hold (100 ms), crossfade out (30 ms) and its detector's frame.
const FAR_PAUSE_FRAMES: usize = 15;

struct ClickQuality {
  mean_loss_db: f32,
  lost6: f32,
}

/// Clean speech at `speech_rms_db` dBFS, the noise clips at `speech_rms_db - snr_db`, and their sum.
fn mix(speech: &str, noise_clips: &[&str], speech_rms_db: f32, snr_db: f32) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
  let clean = read_wav_48k(speech);
  let mut noise: Vec<f32> = noise_clips.iter().flat_map(|f| read_wav_48k(f)).collect();
  while noise.len() < clean.len() {
    noise.extend_from_within(..);
  }
  noise.truncate(clean.len());
  let clean = scale_to_db(&clean, speech_rms_db);
  let noise = scale_to_db(&noise, speech_rms_db - snr_db);
  let mixed = clean.iter().zip(&noise).map(|(s, n)| s + n).collect();
  (clean, noise, mixed)
}

/// Keyboard and mouse clicks only (no speech) at `rms_db` dBFS, 48 kHz.
fn keyboard_only(rms_db: f32) -> Vec<f32> {
  noise_only(&KEYBOARD_CLIPS, rms_db)
}

fn noise_only(noise_clips: &[&str], rms_db: f32) -> Vec<f32> {
  let noise: Vec<f32> = noise_clips.iter().flat_map(|f| read_wav_48k(f)).collect();
  scale_to_db(&noise, rms_db)
}

/// Compares the output (delayed by the model's latency) with the clean speech, per 10 ms frame.
fn measure(clean: &[f32], mixed: &[f32], out: &[f32], latency: usize) -> Quality {
  const FRAME: usize = 480;
  const WARMUP_FRAMES: usize = 50;
  let delayed = &out[latency..];
  let clean_db = db(rms(clean));
  let (mut loud, mut lost, mut soft, mut soft_lost) = (0usize, 0usize, 0usize, 0usize);
  let (mut pause_in, mut pause_out, mut far_in, mut far_out) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
  let near_speech: Vec<bool> = clean.as_chunks::<FRAME>().0.iter().map(|c| db(rms(c)) >= clean_db - 40.0).collect();
  for (i, ((c, m), o)) in clean
    .as_chunks::<FRAME>()
    .0
    .iter()
    .zip(mixed.as_chunks::<FRAME>().0)
    .zip(delayed.as_chunks::<FRAME>().0)
    .enumerate()
    .skip(WARMUP_FRAMES)
  {
    let c_db = db(rms(c));
    let window = i.saturating_sub(FAR_PAUSE_FRAMES)..(i + FAR_PAUSE_FRAMES + 1).min(near_speech.len());
    if c_db < clean_db - 40.0 && !near_speech.get(window).unwrap_or_default().iter().any(|&s| s) {
      far_in += m.iter().map(|x| x * x).sum::<f32>();
      far_out += o.iter().map(|x| x * x).sum::<f32>();
    }
    let is_lost = db(rms(o)) - c_db < -15.0;
    if c_db >= clean_db {
      loud += 1;
      lost += is_lost as usize;
    } else if c_db >= clean_db - 15.0 {
      soft += 1;
      soft_lost += is_lost as usize;
    } else if c_db < clean_db - 40.0 {
      pause_in += m.iter().map(|x| x * x).sum::<f32>();
      pause_out += o.iter().map(|x| x * x).sum::<f32>();
    }
  }
  Quality {
    dropouts: lost as f32 / loud.max(1) as f32,
    soft_dropouts: soft_lost as f32 / soft.max(1) as f32,
    noise_suppression_db: 10.0 * (pause_in.max(1e-12) / pause_out.max(1e-12)).log10(),
    far_suppression_db: 10.0 * (far_in.max(1e-12) / far_out.max(1e-12)).log10(),
  }
}

/// The loss half of `run_clicks`: speech frames (>= clean - 15 dB) with clicks no more than
/// 10 dB below the speech; mean loss and share losing more than 6 dB.
fn click_loss(clean: &[f32], clicks: &[f32], out: &[f32], latency: usize) -> ClickQuality {
  let clean_db = db(rms(clean));
  let (mut n, mut sum, mut lost6) = (0usize, 0.0f32, 0usize);
  for ((c, k), o) in clean
    .as_chunks::<480>()
    .0
    .iter()
    .zip(clicks.as_chunks::<480>().0)
    .zip(out.get(latency..).unwrap_or_default().as_chunks::<480>().0)
    .skip(50)
  {
    let c_db = db(rms(c));
    if c_db >= clean_db - 15.0 && db(rms(k)) > c_db - 10.0 {
      let loss = c_db - db(rms(o));
      n += 1;
      sum += loss.max(0.0);
      lost6 += (loss > 6.0) as usize;
    }
  }
  let n = n.max(1) as f32;
  ClickQuality { mean_loss_db: sum / n, lost6: lost6 as f32 / n }
}

/// WAV fixture as f32 (first channel), linearly resampled to 48 kHz (good enough for test noise).
fn read_wav_48k(name: &str) -> Vec<f32> {
  let mut reader = hound::WavReader::open(format!("{FIXTURES}/{name}")).unwrap();
  let spec = reader.spec();
  let channels = spec.channels.max(1) as usize;
  let samples: Vec<f32> = match spec.sample_format {
    hound::SampleFormat::Int => {
      let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
      reader.samples::<i32>().step_by(channels).map(|s| s.unwrap() as f32 / scale).collect()
    }
    hound::SampleFormat::Float => reader.samples::<f32>().step_by(channels).map(|s| s.unwrap()).collect(),
  };
  if spec.sample_rate == 48_000 {
    return samples;
  }
  let step = spec.sample_rate as f64 / 48_000.0;
  let len = ((samples.len() - 1) as f64 / step) as usize;
  (0..len)
    .map(|i| {
      let pos = i as f64 * step;
      let (j, frac) = (pos as usize, (pos.fract()) as f32);
      samples[j] * (1.0 - frac) + samples[j + 1] * frac
    })
    .collect()
}

fn scale_to_db(x: &[f32], target_db: f32) -> Vec<f32> {
  let gain = 10f32.powf(target_db / 20.0) / rms(x).max(1e-9);
  x.iter().map(|s| s * gain).collect()
}

fn rms(x: &[f32]) -> f32 {
  (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

fn db(x: f32) -> f32 {
  20.0 * x.max(1e-9).log10()
}
