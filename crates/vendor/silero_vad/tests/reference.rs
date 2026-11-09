//! The port against onnxruntime running the upstream model, and the 48 kHz front end on the
//! shared fixtures. Reference files: `tools/convert_weights.py`.
// Tests may panic and index.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use silero_vad::{SileroVad, Vad48k, CHUNK, CHUNK_48K};

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../test_data/denoise");
const KEYBOARD_CLIPS: [&str; 6] = [
  "1-137-A-32.wav",
  "1-53501-A-32.wav",
  "1-118206-A-31.wav",
  "1-61212-A-32.wav",
  "1-18655-A-31.wav",
  "1-79711-A-32.wav",
];

#[test]
fn matches_onnxruntime() {
  let (input, want) = (f32s("silero_reference/in_16k.f32"), f32s("silero_reference/probs.f32"));
  let mut vad = SileroVad::new().unwrap();
  let got: Vec<f32> = input.as_chunks::<CHUNK>().0.iter().map(|c| vad.process_chunk(c)).collect();
  assert_eq!(got.len(), want.len());
  let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
  eprintln!("{} chunks, largest difference to onnxruntime {worst:.2e}", got.len());
  assert!(worst < 1e-4, "{worst}");
  // The reference covers both ends of the range, not one flat stretch.
  assert!(want.iter().any(|&p| p > 0.9) && want.iter().any(|&p| p < 0.05));
}

#[test]
fn reset_starts_the_same_stream_again() {
  let input = f32s("silero_reference/in_16k.f32");
  let mut vad = SileroVad::new().unwrap();
  let run = |vad: &mut SileroVad| -> Vec<f32> {
    input.as_chunks::<CHUNK>().0.iter().take(60).map(|c| vad.process_chunk(c)).collect()
  };
  let first = run(&mut vad);
  vad.reset();
  assert_eq!(first, run(&mut vad));
}

#[test]
fn bad_input_never_sticks() {
  // NaN and infinite samples count as silence; a short chunk is zero-padded.
  let (mut vad, mut clean) = (SileroVad::new().unwrap(), SileroVad::new().unwrap());
  for chunk in [&[f32::NAN; CHUNK][..], &[f32::INFINITY; CHUNK], &[0.0; 10], &[]] {
    let (got, want) = (vad.process_chunk(chunk), clean.process_chunk(&[0.0; CHUNK]));
    assert_eq!(got, want);
    assert!(got < 0.1, "{got}");
  }
}

#[test]
fn hears_speech_and_not_keys_at_48k() {
  // Any block size gives the same chunks.
  let speech = scale_to_db(&wav_48k("speech_clean.wav"), -30.0);
  let by_block = |block: usize| -> Vec<f32> {
    let (mut vad, mut probs) = (Vad48k::new().unwrap(), Vec::new());
    speech.chunks(block).for_each(|b| vad.process(b, |p| probs.push(p)));
    probs
  };
  let probs = by_block(480);
  assert_eq!(probs, by_block(441));
  assert_eq!(probs.len(), speech.len() / CHUNK_48K);

  // Measured: 198 of 199 loud chunks of the clean speech over 0.5; keys alone never over 0.15
  // (highest 0.08).
  let level = rms_db(&speech);
  let (mut loud, mut heard) = (0usize, 0usize);
  for (chunk, &p) in speech.as_chunks::<CHUNK_48K>().0.iter().zip(&probs).skip(10) {
    if rms_db(chunk) >= level - 15.0 {
      loud += 1;
      heard += usize::from(p > 0.5);
    }
  }
  let keys: Vec<f32> = KEYBOARD_CLIPS.iter().flat_map(|f| wav_48k(f)).collect();
  let keys = scale_to_db(&keys, -20.0);
  let (mut vad, mut key_probs) = (Vad48k::new().unwrap(), Vec::new());
  let start = std::time::Instant::now();
  keys.chunks(480).for_each(|b| vad.process(b, |p| key_probs.push(p)));
  let cost = start.elapsed().as_secs_f32() / (keys.len() as f32 / 48_000.0);
  eprintln!("cost with the 48 kHz front end: {:.2} % of real time (meaningful with --release)", cost * 100.0);
  let fired = key_probs.iter().filter(|&&p| p > 0.15).count();
  eprintln!(
    "speech: {heard} of {loud} loud chunks over 0.5; keys alone: {fired} of {} chunks over 0.15 (highest {:.3})",
    key_probs.len(),
    key_probs.iter().copied().fold(0.0f32, f32::max)
  );
  assert!(loud > 100 && heard * 10 >= loud * 9, "{heard} / {loud}");
  assert!(fired * 100 <= key_probs.len(), "{fired} / {}", key_probs.len());
}

fn f32s(name: &str) -> Vec<f32> {
  std::fs::read(format!("{DATA}/{name}")).unwrap().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

/// First channel, linearly resampled to 48 kHz (as `read_wav_48k` in the mic crate's tests).
fn wav_48k(name: &str) -> Vec<f32> {
  let mut reader = hound::WavReader::open(format!("{DATA}/{name}")).unwrap();
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
  (0..((samples.len() - 1) as f64 / step) as usize)
    .map(|i| {
      let pos = i as f64 * step;
      let (j, frac) = (pos as usize, pos.fract() as f32);
      samples[j] * (1.0 - frac) + samples[j + 1] * frac
    })
    .collect()
}

fn rms_db(x: &[f32]) -> f32 {
  10.0 * (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).max(1e-12).log10()
}

fn scale_to_db(x: &[f32], target_db: f32) -> Vec<f32> {
  let gain = 10f32.powf((target_db - rms_db(x)) / 20.0);
  x.iter().map(|s| s * gain).collect()
}
