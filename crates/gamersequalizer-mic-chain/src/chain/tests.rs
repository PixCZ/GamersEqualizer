use super::*;
use gamersequalizer_common::shm::StereoFrame;

/// The whole mic chain (no AEC reference, default mic EQ) on the private recordings, for
/// before/after comparisons of the stages after the denoiser. Per model and input level:
/// output level of voice / whisper / room sections (dBFS, power mean), samples at full scale
/// (>= 0.99, per mille), peak, leveler gain at the end; and keys.wav alone. Run:
/// `cargo test -p gamersequalizer-mic-chain --release chain_report -- --ignored --nocapture`
#[test]
#[ignore = "needs private recordings"]
fn chain_report() {
  use crate::ai_denoiser::tests::read_wav_48k;
  const HALF_SEC: usize = 24_000;
  let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test_data/private");
  if ["whisper_scenario.wav", "keys.wav"].iter().any(|f| !std::path::Path::new(dir).join(f).exists()) {
    return eprintln!("skipped: put whisper_scenario.wav and keys.wav into test_data/private/");
  }
  let scenario = read_wav_48k("../private/whisper_scenario.wav");
  let keys = read_wav_48k("../private/keys.wav");
  let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, s| m.max(s.abs()));
  let db = |p: f32| 10.0 * p.max(1e-12).log10();
  let pow = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32;
  // Default mic EQ of the app: low cut 100 Hz 48 dB/oct, high cut 9 kHz.
  let bands = [
    BiquadBand { filter_type: FilterType::HighPass, frequency: 100.0, q: 0.707, slope_db: 48, ..BiquadBand::default() },
    BiquadBand {
      filter_type: FilterType::LowPass,
      frequency: 9_000.0,
      q: 0.707,
      slope_db: 12,
      ..BiquadBand::default()
    },
  ];
  // (output, leveler gain at the end, share of blocks the heavy model slept, whisper section only)
  let run = |model: DenoiseModel, permille: u32, input: &[f32]| -> (Vec<f32>, f32, f32) {
    let mut p = MicChain::with_shm(None);
    p.test_overrides = Some((model, permille));
    p.ai_denoiser.set_model(model);
    p.eq.set_bands(&bands, 48_000.0);
    let mut out = vec![0.0f32; input.len()];
    let mut asleep = 0usize;
    let whisper_blocks = 40 * HALF_SEC / 480..48 * HALF_SEC / 480;
    for (n, (i, o)) in input.chunks(480).zip(out.chunks_mut(480)).enumerate() {
      p.process_timed(&[i], &mut [o], None);
      asleep += usize::from(whisper_blocks.contains(&n) && p.ai_denoiser.asleep());
    }
    let lead = p.ai_denoiser.output_lead_samples();
    let slept = asleep as f32 * 100.0 / whisper_blocks.len() as f32;
    (out.get(lead..).unwrap_or_default().to_vec(), p.leveler.gain_db(), slept)
  };
  let scale = |x: &[f32], g_db: f32| -> Vec<f32> { x.iter().map(|s| s * 10f32.powf(g_db / 20.0)).collect() };
  // Desk rumble: 45 Hz at -30 dBFS, swelling at 0.3 Hz.
  let rumble: Vec<f32> = scenario
    .iter()
    .enumerate()
    .map(|(i, s)| {
      let t = i as f32 / 48_000.0;
      s + 0.0316 * (0.5 + 0.5 * (std::f32::consts::TAU * 0.3 * t).sin()) * (std::f32::consts::TAU * 45.0 * t).sin()
    })
    .collect();
  let hot_db = db(0.89f32.powi(2) / peak(&scenario).max(1e-6).powi(2));
  // A mic set far too hot: voice ~-10 dBFS, the converter clips the peaks.
  let clipped_in: Vec<f32> = scale(&scenario, 20.0).iter().map(|s| s.clamp(-1.0, 1.0)).collect();
  let variants: [(&str, Vec<f32>, u32); 8] = [
    ("-24 dB", scale(&scenario, -24.0), 1000),
    ("-12 dB", scale(&scenario, -12.0), 1000),
    ("0 dB", scenario.clone(), 1000),
    ("hot -1 dBFS pk", scale(&scenario, hot_db), 1000),
    ("+20 dB clipped", clipped_in, 1000),
    ("0 dB boost 200%", scenario.clone(), 2000),
    ("0 dB boost 300%", scenario.clone(), 3000),
    ("0 dB + rumble", rumble, 1000),
  ];
  // Pauses once the leveler has learned: 10th percentile of the 10 ms frame levels after 30 s
  // (digital silence left out).
  let pauses = |x: &[f32]| {
    let mut l: Vec<f32> =
      x.get(60 * HALF_SEC..).unwrap_or_default().chunks(480).map(|f| db(pow(f))).filter(|l| *l > -119.0).collect();
    l.sort_by(f32::total_cmp);
    l.get(l.len() / 10).copied().unwrap_or(-120.0)
  };
  let sec = |x: &[f32], a: usize, b: usize| db(pow(x.get(a * HALF_SEC..b * HALF_SEC).unwrap_or_default()));
  eprintln!(
    "input {:+.1} dB = hot; sections dBFS: voice 45-49 s | voice 53-57 s | whisper 20-24 s | room 1-18 s",
    hot_db
  );
  for model in [DenoiseModel::DeepFilterNet, DenoiseModel::RnNoise] {
    for (name, input, permille) in &variants {
      let (out, gain, slept) = run(model, *permille, input);
      let full = out.iter().filter(|s| s.abs() >= 0.99).count() as f32 * 1000.0 / out.len().max(1) as f32;
      eprintln!(
        "{model:<13?} {name:<16} voice {:6.1} {:6.1} whisper {:6.1} room {:6.1} pauses {:6.1} | full scale {:6.2} permille, peak {:5.2} dBFS | leveler {:+5.1} dB | asleep in whisper {:3.0} %",
        sec(&out, 90, 98),
        sec(&out, 107, 115),
        sec(&out, 40, 48),
        sec(&out, 2, 36),
        pauses(&out),
        full,
        20.0 * peak(&out).max(1e-6).log10(),
        gain,
        slept
      );
    }
    let (out, _, _) = run(model, 1000, &keys);
    eprintln!("{model:<13?} keys.wav alone: in {:6.1} dBFS, out {:6.1} dBFS", db(pow(&keys)), db(pow(&out)));
  }
}

#[test]
fn test_db_x10() {
  assert_eq!(db_x10(1.0), 0);
  assert_eq!(db_x10(0.1), -200);
  assert_eq!(db_x10(0.0), -1200);
  assert_eq!(db_x10(f32::INFINITY), -1200);
  assert_eq!(db_x10(f32::NAN), -1200);
  assert_eq!(db_x10(1e30), 200);
}

#[test]
fn test_echo_strength_sign_says_measured_or_bound() {
  assert_eq!(echo_db_x10(None), 0);
  assert_eq!(echo_db_x10(Some((-25.04, false))), -250);
  assert_eq!(echo_db_x10(Some((-25.04, true))), 250);
  assert_eq!(echo_db_x10(Some((-0.01, false))), -1, "a measured 0 dB is not \"none\"");
  assert_eq!(echo_db_x10(Some((-0.01, true))), 1);
  assert_eq!(echo_db_x10(Some((f32::NEG_INFINITY, true))), 1200);
}

/// Another speaker output (`aec_reference_seq`): the delay from the old one is dropped and the
/// new output's remembered delay is taken, which the daemon stored before the switch.
#[test]
fn test_other_speakers_start_from_their_remembered_delay() {
  let mut p = chain_with_reference("reference_seq", &noise(120_000, 5));
  let h = p.shm_header().expect("mapping");
  h.aec_cached_delay_samples.store(960, Ordering::Relaxed);
  let (mic, mut out) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
  p.process_timed(&[&mic], &mut [&mut out], Some(block_hns(0)));
  assert_eq!(p.delay_estimator.current_delay(), Some(960));
  let h = p.shm_header().expect("mapping");
  h.aec_cached_delay_samples.store(1_200, Ordering::Relaxed);
  p.process_timed(&[&mic], &mut [&mut out], Some(block_hns(1)));
  assert_eq!(p.delay_estimator.current_delay(), Some(960), "the same output keeps its delay");
  p.shm_header().expect("mapping").aec_reference_seq.fetch_add(1, Ordering::Release);
  for k in 2..12 {
    p.process_timed(&[&mic], &mut [&mut out], Some(block_hns(k)));
  }
  assert_eq!(p.delay_estimator.current_delay(), Some(1_200));
  assert!(p.delay_estimator.is_from_cache());
  let seen = p.shm_header().expect("mapping").aec_reference_seq_seen.load(Ordering::Acquire);
  assert_eq!(seen, 1, "published for the daemon");
}

#[test]
fn test_smooth_rms_recovers_from_non_finite_block() {
  // Regression: one inf sample pinned the speaker meter at "214748364.7" for good.
  let rms = smooth_rms(0.1, &[f32::INFINITY, 0.0]);
  assert_eq!(rms, 0.1);
  assert_eq!(smooth_rms(f32::INFINITY, &[f32::NAN]), 0.0);
  assert!((smooth_rms(rms, &[0.1; 4]) - 0.1).abs() < 1e-6);
}

#[test]
fn test_overload_needs_consecutive_windows_while_deepfilter_runs() {
  let window = |load: &mut DspLoad, share: f32, df: bool| {
    for _ in 0..TELEMETRY_EVERY_BLOCKS {
      load.add(share);
    }
    let fall_back = load.end_window(df);
    load.reset_window();
    fall_back
  };
  let mut load = DspLoad::default();
  // RNNoise slow: reported (late blocks) but nothing to fall back from.
  assert!(!(0..5).any(|_| window(&mut load, 1.2, false)));
  assert_eq!(load.late_total, 5 * TELEMETRY_EVERY_BLOCKS);
  // A short spike is tolerated, a sustained overload is not.
  assert!(!window(&mut load, 1.5, true));
  assert!(!window(&mut load, 0.1, true));
  assert!(!window(&mut load, 0.95, true));
  assert!(!window(&mut load, 0.95, true));
  assert!(window(&mut load, 0.95, true));
}

/// Regression: the delay estimator got the raw input, so one NaN sample made a whole 1 s
/// window read as "no echo" instead of measuring the echo in it.
#[test]
fn test_nan_in_the_mic_does_not_spoil_the_delay_window() {
  const ECHO_DELAY: usize = 2_000;
  let reference = noise(120_000, 1);
  let mut p = chain_with_reference("nan_delay", &reference);
  let mut out = vec![0.0f32; BLOCK];
  let mut k = 0;
  while p.delay_estimator.last_outcome().is_none() {
    assert!(k < 200, "no measurement window finished");
    let t = block_hns(k);
    let start = ref_position(&p, t, reference.len());
    let mut mic: Vec<f32> =
      (0..BLOCK).map(|i| 0.3 * (start + i).checked_sub(ECHO_DELAY).map_or(0.0, |j| reference[j])).collect();
    if k == 3 {
      mic[100] = f32::NAN;
    }
    p.process_timed(&[&mic], &mut [&mut out], Some(t));
    k += 1;
  }
  let outcome = p.delay_estimator.last_outcome();
  assert!(matches!(outcome, Some(WindowOutcome::Estimate { .. })), "{outcome:?}");
  assert!(p.delay_estimator.corr_curve().iter().all(|v| v.is_finite()));
}

/// Regression: AEC off -> on replayed the samples AEC3 had queued before it was switched off.
#[test]
fn test_aec_switched_back_on_drops_audio_queued_before() {
  let mut p = chain_with_reference("aec_idle", &noise(120_000, 2));
  p.shm_header().expect("mapping").aec_cached_delay_samples.store(960, Ordering::Relaxed); // AEC runs at once
  let (loud, mut out) = (noise(BLOCK, 3), vec![0.0f32; BLOCK]);
  for k in 0..20 {
    p.process_timed(&[&loud], &mut [&mut out], Some(block_hns(k)));
  }
  assert!(!p.aec_idle, "the AEC ran ({:?})", p.aec_state);
  p.shm_header().expect("mapping").aec_enabled.store(false, Ordering::Relaxed);
  p.process_timed(&[&loud], &mut [&mut out], Some(block_hns(20)));
  p.shm_header().expect("mapping").aec_enabled.store(true, Ordering::Relaxed);
  // The AEC stage alone (later stages keep their own history): silence in, silence out. Two
  // whole frames: the first frame AEC3 puts out after the gap starts with its own delay line.
  for k in 0..2 {
    p.mic_mono_temp[..FRAME_480].fill(0.0);
    p.run_aec(FRAME_480, Some(block_hns(21) + k * 100_000));
    let peak = p.mic_mono_temp[..FRAME_480].iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak < 1e-3, "block {k}: samples from before the switch came out (peak {peak})");
  }
}

/// Safety net for changes that must not change the sound: speech, keys and an echo of the
/// reference through the whole chain (AEC, RNNoise, leveler, limiter), with a pause, a master
/// bypass, AEC off and on and "Learn again" on the way. Energy per second of the output must
/// stay what it was; a change that is meant to alter the sound updates `EXPECTED` with a reason.
#[test]
fn test_whole_chain_output_stays_the_same() {
  use crate::ai_denoiser::tests::read_wav_48k;
  const SECONDS: usize = 8;
  const ECHO_DELAY: usize = 960;
  // G1 (2026-11-16): seconds 2.. changed, AEC3's first frame after a pause is silenced.
  const EXPECTED: [f64; SECONDS] = [5.0476, 568.84, 113.22, 74.089, 196.25, 172.34, 67.444, 68.037];
  let speech = read_wav_48k("speech_clean.wav");
  let keys = read_wav_48k("1-137-A-32.wav");
  let reference: Vec<f32> = noise(SECONDS * 48_000 + 48_000, 7).iter().map(|s| s * 0.4).collect();
  let mut p = chain_with_reference("golden", &reference);
  let header = p.shm_header().expect("mapping");
  header.aec_cached_delay_samples.store(ECHO_DELAY as u32, Ordering::Relaxed);
  let mut energy = [0.0f64; SECONDS];
  let mut out = vec![0.0f32; FRAME_480];
  for k in 0..SECONDS * 100 {
    let t = T0_HNS + 5_000_000 + (k * FRAME_480) as u64 * 10_000_000 / 48_000;
    let start = ref_position(&p, t, reference.len());
    let mic: Vec<f32> = (0..FRAME_480)
      .map(|i| {
        let n = k * FRAME_480 + i;
        let echo = (start + i).checked_sub(ECHO_DELAY).map_or(0.0, |j| reference[j]);
        speech[n % speech.len()] + 0.5 * keys[n % keys.len()] + 0.2 * echo
      })
      .collect();
    let h = p.shm_header().expect("mapping");
    match k {
      200 => p.pause(),
      300..305 => h.master_bypass.store(true, Ordering::Relaxed),
      400..410 => h.aec_enabled.store(false, Ordering::Relaxed),
      500 => _ = h.mic_leveler_reset_seq.fetch_add(1, Ordering::Relaxed),
      _ => {
        h.master_bypass.store(false, Ordering::Relaxed);
        h.aec_enabled.store(true, Ordering::Relaxed);
      }
    }
    p.process_timed(&[&mic], &mut [&mut out], Some(t));
    energy[k / 100] += out.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>();
  }
  eprintln!("energy per second: {energy:?}");
  for (second, (got, want)) in energy.iter().zip(EXPECTED).enumerate() {
    assert!((got - want).abs() <= 1e-3 * want.abs().max(1e-6), "second {second}: {got} vs {want}");
  }
}

/// The whole chain as a gamer runs it, which the test above does not reach: DeepFilterNet 75 %,
/// gate auto, leveler on at -24 dBFS, the Windows mic at 30 % (moved to 50 % once, the ramp), a
/// voice EQ with a steep low cut, AEC with an echo. Energy per second must stay what it was, and
/// every word onset must reach the output one chain latency later, not cut against the word.
#[test]
fn test_gamer_chain_output_and_word_onsets_stay_the_same() {
  use crate::ai_denoiser::tests::read_wav_48k;
  const SECONDS: usize = 8;
  const ECHO_DELAY: usize = 960;
  // G1 (2026-11-24): first values.
  const EXPECTED: [f64; SECONDS] = [0.00031116, 4.9425, 2.9742, 4.2844, 9.1842, 42.469, 14.043, 14.466];
  // Onset cut measured -2.0 .. +3.5 dB; a gate or model opening 10-30 ms late cuts tens of dB.
  const MAX_ONSET_CUT_DB: f32 = 6.0;
  let speech = read_wav_48k("speech_clean.wav");
  let keys = read_wav_48k("1-137-A-32.wav");
  let reference: Vec<f32> = noise(SECONDS * 48_000 + 48_000, 7).iter().map(|s| s * 0.4).collect();
  let band = |filter_type, frequency, gain_db, q, slope_db| BiquadBand {
    enabled: true,
    filter_type,
    frequency,
    gain_db,
    q,
    slope_db,
  };
  // The app's "Male voice" mic profile.
  let bands = [
    band(FilterType::HighPass, 80.0, 0.0, 0.707, 48),
    band(FilterType::Peaking, 140.0, 2.0, 1.0, 12),
    band(FilterType::Peaking, 400.0, -2.5, 1.2, 12),
    band(FilterType::Peaking, 3000.0, 3.0, 1.2, 12),
    band(FilterType::LowPass, 12000.0, 0.0, 0.707, 24),
  ];
  let mut p = chain_with_reference("gamer", &reference);
  p.test_overrides = None;
  p.ai_denoiser.set_model(DenoiseModel::DeepFilterNet);
  {
    let h = p.shm_header().expect("mapping");
    let r = Ordering::Relaxed;
    h.aec_cached_delay_samples.store(ECHO_DELAY as u32, r);
    h.denoise_model.store(DenoiseModel::DeepFilterNet.to_u32(), r);
    h.ai_voice_strength_permille.store(750, r);
    h.noise_gate_threshold_db.store(0, r);
    h.mic_leveler_enabled.store(true, r);
    h.mic_leveler_target_db_x10.store(-240, r);
    h.mic_input_gain_db_x10.store(-43, r);
    h.mic_volume_permille.store(300, r);
    h.write_mic_eq(&bands);
  }
  // Every second: 400 ms of keys and echo only, then 600 ms of speech: a word onset at a known
  // block, against keys the gate must stay shut for.
  const GAP_BLOCKS: usize = 40;
  let talking = |n: usize| n % 48_000 >= GAP_BLOCKS * FRAME_480;
  let blocks = SECONDS * 100;
  let mut energy = [0.0f64; SECONDS];
  let mut out_all = vec![0.0f32; blocks * FRAME_480];
  let mut out = vec![0.0f32; FRAME_480];
  for k in 0..blocks {
    let t = T0_HNS + 5_000_000 + (k * FRAME_480) as u64 * 10_000_000 / 48_000;
    let start = ref_position(&p, t, reference.len());
    let mic: Vec<f32> = (0..FRAME_480)
      .map(|i| {
        let n = k * FRAME_480 + i;
        let echo = (start + i).checked_sub(ECHO_DELAY).map_or(0.0, |j| reference[j]);
        let voice = if talking(n) { speech[n % speech.len()] } else { 0.0 };
        voice + 0.5 * keys[n % keys.len()] + 0.2 * echo
      })
      .collect();
    if k == 500 {
      p.shm_header().expect("mapping").mic_volume_permille.store(500, Ordering::Relaxed);
    }
    p.process_timed(&[&mic], &mut [&mut out], Some(t));
    energy[k / 100] += out.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>();
    out_all[k * FRAME_480..(k + 1) * FRAME_480].copy_from_slice(&out);
  }
  eprintln!("energy per second: {energy:?}");

  // Each word after the first second (leveler and model warmed up), found in the clean speech: its
  // first 30 ms against the 100 ms after them, on the input and one chain latency later on the
  // output. A gate or model that opens late cuts the onset against the word.
  let db = |s: &[f32]| 10.0 * (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).max(1e-12).log10();
  let frame_in =
    |f: usize| db(&(f * FRAME_480..(f + 1) * FRAME_480).map(|n| speech[n % speech.len()]).collect::<Vec<_>>());
  let frame_out = |f: usize| db(&out_all[f * FRAME_480..(f + 1) * FRAME_480]);
  let max_of = |frame: &dyn Fn(usize) -> f32, r: std::ops::Range<usize>| r.map(frame).fold(f32::MIN, f32::max);
  let lag = p.latency_stages.div_ceil(FRAME_480);
  let mut words = 0;
  for second in 1..SECONDS {
    let burst = second * 100 + GAP_BLOCKS;
    let Some(f) = (burst..burst + 30).find(|&f| frame_in(f) > -40.0) else { continue };
    let shape_in = max_of(&frame_in, f..f + 3) - max_of(&frame_in, f + 3..f + 13);
    let g = f + lag;
    let shape_out = max_of(&frame_out, g..g + 3) - max_of(&frame_out, g + 3..g + 13);
    let cut = shape_in - shape_out;
    assert!(cut <= MAX_ONSET_CUT_DB, "word at {:.2} s: onset cut {cut:.1} dB (lag {lag} blocks)", f as f32 / 100.0);
    words += 1;
  }
  assert!(words >= 5, "only {words} word onsets found");
  for (second, (got, want)) in energy.iter().zip(EXPECTED).enumerate() {
    assert!((got - want).abs() <= 1e-3 * want.abs().max(1e-6), "second {second}: {got} vs {want}");
  }
}

/// "Learn again" blocks the saved level until the daemon clears it; a level it saves after that
/// is the mic's again and seeds the leveler (regression: blocked for the rest of the stream).
#[test]
fn test_saved_level_seeds_again_once_the_daemon_cleared_it() {
  let mut p = chain_with_reference("seed", &noise(4_800, 4));
  let silence = [0.0f32; FRAME_480];
  let mut out = [0.0f32; FRAME_480];
  let mut block = |p: &mut MicChain, saved_x10: i32| {
    p.shm_header().expect("mapping").mic_leveler_saved_level_db_x10.store(saved_x10, Ordering::Relaxed);
    p.process_timed(&[&silence], &mut [&mut out], None);
    p.leveler.level_db().map(|db| db.round())
  };
  assert_eq!(block(&mut p, -200), Some(-20.0), "seeded");
  p.shm_header().expect("mapping").mic_leveler_reset_seq.fetch_add(1, Ordering::Relaxed);
  assert_eq!(block(&mut p, -200), None, "the stale level is not taken back");
  p.shm_header().expect("mapping").mic_leveler_enabled.store(false, Ordering::Relaxed);
  assert_eq!(block(&mut p, 0), None, "cleared by the daemon");
  p.shm_header().expect("mapping").mic_leveler_enabled.store(true, Ordering::Relaxed);
  assert_eq!(block(&mut p, -150), Some(-15.0), "a level saved since seeds again");
}

const FRAME_480: usize = 480;

/// Blocks that don't line up with AEC3's 480-sample frames, so it queues output.
const BLOCK: usize = 441;
const T0_HNS: u64 = 10_000_000_000;

/// Chain on a private mapping whose reference played `reference` from `T0_HNS`;
/// RNNoise, 100 % volume.
fn chain_with_reference(tag: &str, reference: &[f32]) -> MicChain {
  let name = format!("Local\\GamersEqualizer_test_{tag}_{}", std::process::id());
  let mut shm = SharedMemoryClient::create_named(&name, None).expect("test mapping");
  let frames: Vec<StereoFrame> = reference.iter().map(|&s| StereoFrame { left: s, right: s }).collect();
  shm.ring_buffer().push_frames_timed(&frames, T0_HNS);
  shm.layout().header.tap_active.store(true, Ordering::Relaxed); // the reference writer's flag
  let mut p = MicChain::with_shm(Some(shm));
  p.test_overrides = Some((DenoiseModel::RnNoise, 1000));
  p
}

/// QPC time of mic block `k`, from 0.5 s into the reference.
fn block_hns(k: usize) -> u64 {
  T0_HNS + 5_000_000 + (k * BLOCK) as u64 * 10_000_000 / 48_000
}

/// Position in the reference (`len` samples, all written) that played at `t`.
fn ref_position(p: &MicChain, t: u64, len: usize) -> usize {
  let h = p.shm_header().expect("test mapping");
  let first = h.ref_snap_index.load(Ordering::Relaxed).wrapping_sub(len as u32);
  h.ref_index_at(t).expect("reference").wrapping_sub(first) as usize
}

fn noise(len: usize, seed: u32) -> Vec<f32> {
  let mut x = seed;
  (0..len)
    .map(|_| {
      x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
      ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.5
    })
    .collect()
}
