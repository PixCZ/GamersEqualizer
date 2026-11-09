//! Spec 6.2/6.4: the port against upstream's C output, and the latency constant.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, reason = "tests")]

use rnnoise2::{DenoiseState, Model, FRAME_SIZE, LATENCY_FRAMES};

const REFERENCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../test_data/denoise/rnnoise2_reference");
/// Spec 6.2 thresholds: 1e-3 and 1e-4 of full scale, in LSB.
const MAX_ERR_LSB: f64 = 1e-3 * 32768.0;
const RMS_ERR_LSB: f64 = 1e-4 * 32768.0;

fn read_i16(path: &str) -> Vec<i16> {
  let mut reader = hound::WavReader::open(path).expect(path);
  reader.samples::<i16>().map(|s| s.unwrap()).collect()
}

/// Runs whole frames through a fresh state like `rnnoise_demo`: `(short)` cast (truncation),
/// trailing partial frame dropped. Returns all output samples (the demo drops the first frame).
fn run(model: Model, input: &[i16]) -> Vec<i16> {
  let mut state = DenoiseState::with_model(model).unwrap();
  let mut out = Vec::with_capacity(input.len());
  let mut frame_out = [0.0f32; FRAME_SIZE];
  for chunk in input.as_chunks::<FRAME_SIZE>().0 {
    let frame: Vec<f32> = chunk.iter().map(|&s| s as f32).collect();
    state.process_frame(&mut frame_out, &frame);
    out.extend(frame_out.iter().map(|&s| s as i16));
  }
  out
}

#[test]
fn reference_matches_upstream() {
  let mut failures = Vec::new();
  for clip in ["speech", "keys", "clicks"] {
    let input = read_i16(&format!("{REFERENCE}/in_{clip}.wav"));
    for (model, name) in [(Model::Full, "full"), (Model::Little, "little")] {
      let reference = read_i16(&format!("{REFERENCE}/out_{clip}_{name}.wav"));
      let ours = run(model, &input);
      // Reference sample n = our output sample n + FRAME_SIZE.
      let ours = &ours[FRAME_SIZE..];
      assert_eq!(ours.len(), reference.len(), "{clip} {name}: length");
      let (mut max, mut sq) = (0.0f64, 0.0f64);
      for (&a, &b) in ours.iter().zip(&reference) {
        let d = (a as f64 - b as f64).abs();
        max = max.max(d);
        sq += d * d;
      }
      let rms = (sq / reference.len() as f64).sqrt();
      println!("{clip:6} {name:6} max {max:7.1} LSB  rms {rms:7.3} LSB");
      if max > MAX_ERR_LSB || rms > RMS_ERR_LSB {
        failures.push(format!("{clip} {name}: max {max:.1} LSB, rms {rms:.3} LSB"));
      }
    }
  }
  assert!(failures.is_empty(), "over the spec 6.2 thresholds: {failures:?}");
}

#[test]
fn latency_matches_constant() {
  let delay = LATENCY_FRAMES * FRAME_SIZE;
  for model in [Model::Full, Model::Little] {
    // 1) A tiny impulse in digital silence stays on the silence path (band energy < 0.04, no
    //    gain), where the output is the high-passed input delayed exactly: the peak moves by `delay`.
    let mut input = vec![0.0f32; 10 * FRAME_SIZE];
    let at = 3 * FRAME_SIZE + 123;
    input[at] = 0.01;
    let out = process(model, &input);
    let peak = (0..out.len()).max_by(|&a, &b| out[a].abs().total_cmp(&out[b].abs())).unwrap();
    assert_eq!(peak - at, delay, "{model:?}: impulse");
    // 2) Loud white noise runs the network; the cross-correlation peak still sits at `delay`.
    let mut seed = 0x9e37_79b9u32;
    let noise: Vec<f32> = (0..100 * FRAME_SIZE)
      .map(|_| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed as f32 / u32::MAX as f32 - 0.5) * 6000.0
      })
      .collect();
    let out = process(model, &noise);
    let xcorr = |lag: usize| -> f64 { noise.iter().zip(&out[lag..]).map(|(&a, &b)| a as f64 * b as f64).sum() };
    let best = (0..3 * FRAME_SIZE).max_by(|&a, &b| xcorr(a).total_cmp(&xcorr(b))).unwrap();
    assert_eq!(best, delay, "{model:?}: noise cross-correlation");
  }
}

/// `reset()` must clear every piece of signal and network state: after it, a state that has seen
/// another clip produces bit-identical output to a fresh instance.
#[test]
fn reset_restores_a_fresh_state() {
  let to_f32 = |v: Vec<i16>| v.into_iter().map(f32::from).collect::<Vec<f32>>();
  let speech = to_f32(read_i16(&format!("{REFERENCE}/in_speech.wav")));
  let keys = to_f32(read_i16(&format!("{REFERENCE}/in_keys.wav")));
  for model in [Model::Full, Model::Little] {
    let fresh = process(model, &speech);
    let mut state = DenoiseState::with_model(model).unwrap();
    let mut out = vec![0.0f32; speech.len()];
    for (i, o) in keys.as_chunks::<FRAME_SIZE>().0.iter().zip(out.as_chunks_mut::<FRAME_SIZE>().0) {
      state.process_frame(o, i);
    }
    state.reset();
    for (i, o) in speech.as_chunks::<FRAME_SIZE>().0.iter().zip(out.as_chunks_mut::<FRAME_SIZE>().0) {
      state.process_frame(o, i);
    }
    assert_eq!(fresh, out, "{model:?}: output after reset() differs from a fresh state");
  }
}

/// `voice_probability` evolves the network exactly as `process_frame`: same VAD, bit for bit.
#[test]
fn voice_probability_matches_process_frame() {
  let keys: Vec<f32> = read_i16(&format!("{REFERENCE}/in_keys.wav")).into_iter().map(f32::from).collect();
  for model in [Model::Full, Model::Little] {
    let mut full = DenoiseState::with_model(model).unwrap();
    let mut vad_only = DenoiseState::with_model(model).unwrap();
    let mut out = [0.0f32; FRAME_SIZE];
    let mut voiced = 0;
    for (n, frame) in keys.as_chunks::<FRAME_SIZE>().0.iter().enumerate() {
      let expected = full.process_frame(&mut out, frame);
      let got = vad_only.voice_probability(frame);
      assert_eq!(got.to_bits(), expected.to_bits(), "{model:?} frame {n}");
      voiced += usize::from(expected > 0.5);
    }
    assert!(voiced > 0, "{model:?}: no voiced frame, the comparison proves little");
  }
}

fn process(model: Model, input: &[f32]) -> Vec<f32> {
  let mut state = DenoiseState::with_model(model).unwrap();
  let mut out = vec![0.0f32; input.len()];
  for (i, o) in input.as_chunks::<FRAME_SIZE>().0.iter().zip(out.as_chunks_mut::<FRAME_SIZE>().0) {
    state.process_frame(o, i);
  }
  out
}
