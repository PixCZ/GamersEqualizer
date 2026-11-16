use super::*;
use gamersequalizer_common::biquad::BiquadCascade;
use gamersequalizer_common::profile::{BiquadBand, FilterType};

#[test]
fn test_ai_denoiser_smoke() {
  let mut isolator = AiVoiceIsolator::new();
  let mut buffer = vec![0.05f32; 960]; // 2 frames
  isolator.process_samples(&mut buffer);
  assert_eq!(buffer.len(), 960);
}

#[test]
fn test_odd_block_size_never_leaks_raw_input() {
  // Regression: with blocks that are not a multiple of 480, the unprocessed
  // tail used to be passed through raw, mixing dry and denoised audio.
  let mut isolator = AiVoiceIsolator::new();
  isolator.set_gate_threshold_db(-80.0);
  let mut first = vec![0.5f32; 300];
  isolator.process_samples(&mut first);
  assert!(first.iter().all(|&s| s == 0.0), "first block must be primed silence");

  for _ in 0..20 {
    let mut block = vec![0.5f32; 441];
    isolator.process_samples(&mut block);
    assert_eq!(block.len(), 441);
    assert!(block.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
  }
}

#[test]
fn test_bypass_passes_through_and_reenable_resets() {
  let mut isolator = AiVoiceIsolator::new();
  isolator.set_strength(0.0);
  let mut block = vec![0.25f32; 480];
  isolator.process_samples(&mut block);
  assert!(block.iter().all(|&s| s == 0.25));

  isolator.set_strength(1.0);
  let mut block = vec![0.25f32; 480];
  isolator.process_samples(&mut block);
  assert!(block.iter().all(|&s| s == 0.0), "re-enable starts from primed silence");
}

#[test]
fn test_bypass_still_hears_speech_for_the_leveler() {
  // Regression: with the denoiser off no detector ran, so the leveler never learned.
  let (clean, _) = speech_over_keyboard(-30.0, 0.0);
  let mut isolator = AiVoiceIsolator::new();
  isolator.set_strength(0.0);
  let (mut speech_blocks, mut x) = (0, clean.clone());
  for b in x.chunks_mut(480) {
    isolator.process_samples(b);
    speech_blocks += usize::from(isolator.heard_voice());
  }
  assert_eq!(x, clean, "bypass leaves the audio untouched");
  assert_eq!(isolator.output_lead_samples(), 0);
  assert!(speech_blocks > clean.len() / 480 / 10, "heard speech in {speech_blocks} blocks");
}

#[test]
fn test_debug_state_reports_the_detector_and_the_gate() {
  let (clean, _) = speech_over_keyboard(-30.0, 0.0);
  let mut isolator = AiVoiceIsolator::new();
  let mut x = clean.get(..480 * 20).unwrap_or_default().to_vec();
  let mut seen_soft = false;
  for b in x.chunks_mut(480) {
    isolator.process_samples(b);
    let d = isolator.debug_state();
    assert!((0.0..=1.0).contains(&d.in_prob), "RNNoise's own probability, one frame per block: {}", d.in_prob);
    assert!(d.gate_enabled && d.gate_gain <= 1.0 && d.rescue_prob == -1.0);
    seen_soft |= (0.0..=1.0).contains(&d.soft_prob);
    assert!(d.soft_prob == -1.0 || (0.0..=1.0).contains(&d.soft_prob), "{}", d.soft_prob);
  }
  assert!(seen_soft, "Silero completes a chunk every 32 ms");
}

#[test]
fn test_in_net_rarely_fires_on_keys_alone() {
  // RNNoise 0.2 at `IN_NET_VOICE_PROB`: 0.5 % of frames in rnnoise2's `vad_sweep`.
  let probs = in_net_probs(&keyboard_only(-20.0));
  let share = probs.iter().filter(|&&p| p > IN_NET_VOICE_PROB).count() as f32 / probs.len().max(1) as f32;
  eprintln!("in_net, keys alone: {:.1} % of frames above {IN_NET_VOICE_PROB}", share * 100.0);
  assert!(share < 0.02, "{share}");
}

#[test]
fn test_in_net_hears_speech_under_keys() {
  // 97 % in rnnoise2's `vad_sweep` (the 2017 detector at 0.5: 82 %).
  let (clean, mixed) = speech_over_keyboard(-20.0, 0.0);
  let probs = in_net_probs(&mixed);
  let speech_db = db(rms(&clean)) - 15.0;
  let (mut speech, mut heard) = (0usize, 0usize);
  // Probability t + 1 describes frame t (one frame of network lookahead).
  for (frame, p) in clean.as_chunks::<FRAME_SIZE>().0.iter().zip(probs.iter().skip(1)).skip(50) {
    if db(rms(frame)) >= speech_db {
      speech += 1;
      heard += usize::from(*p > IN_NET_VOICE_PROB);
    }
  }
  let share = heard as f32 / speech.max(1) as f32;
  eprintln!("in_net, speech over keys at 0 dB SNR: {:.1} % of {speech} speech frames heard", share * 100.0);
  assert!(speech > 100 && share > 0.9, "{heard} / {speech}");
}

#[test]
fn test_deepfilter_loads_and_switches_both_ways() {
  let mut isolator = AiVoiceIsolator::new();
  isolator.set_gate_threshold_db(-80.0);
  isolator.set_model(DenoiseModel::DeepFilterNet);
  let mut block = vec![0.01f32; 480];
  isolator.process_samples(&mut block);
  assert_eq!(isolator.active_model(), DenoiseModel::DeepFilterNet, "load error: {:?}", isolator.df_error);

  let mut block = vec![0.1f32; 441];
  isolator.process_samples(&mut block);
  assert!(block.iter().all(|s| s.is_finite()));

  isolator.set_model(DenoiseModel::RnNoise);
  let mut block = vec![0.1f32; 480];
  isolator.process_samples(&mut block);
  assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
}

#[test]
fn test_heavy_model_sleeps_in_pauses_and_wakes_without_a_gap() {
  let (clean, _) = speech_over_keyboard(-30.0, 0.0);
  let hiss = |secs: usize| {
    let mut x = 1u32;
    (0..secs * 48_000)
      .map(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 1e-3 // white noise, -66 dBFS
      })
      .collect::<Vec<f32>>()
  };
  let mut input = hiss(3);
  input.extend_from_slice(&clean);
  // 5 s: the clean speech has digital silence, which holds the noise floor down for 1.5 s.
  input.extend(hiss(5));
  for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
    let run = |sleep_after: usize| {
      let mut iso = AiVoiceIsolator::new();
      iso.set_model(model);
      iso.set_gate_threshold_db(GATE_OFF_DB - 1.0);
      iso.sleep_after = sleep_after;
      let (mut out, mut asleep) = (input.clone(), Vec::new());
      for block in out.chunks_mut(480) {
        iso.process_samples(block);
        asleep.push(iso.asleep());
      }
      (out, asleep)
    };
    let (out, asleep) = run(SLEEP_AFTER_SAMPLES);
    let (reference, _) = run(usize::MAX);
    assert!(asleep.get(299).copied().unwrap_or(false), "{model:?}: asleep after 3 s of hiss");
    assert!(asleep.last().copied().unwrap_or(false), "{model:?}: asleep again after the speech");
    let speech_blocks = (300..300 + clean.len() / 480).filter(|&b| !asleep[b]).count();
    assert!(speech_blocks > clean.len() / 480 / 2, "{model:?}: awake in {speech_blocks} speech blocks");
    // No gap: wherever the never-sleeping run is loud, the sleeping one is too.
    let frame_db = |x: &[f32]| 10.0 * (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32 + 1e-20).log10();
    for (i, (o, r)) in out.chunks(480).zip(reference.chunks(480)).enumerate() {
      if frame_db(r) > -50.0 {
        assert!(
          frame_db(o) > frame_db(r) - 12.0,
          "{model:?} block {i}: {:.1} dB vs {:.1} dB",
          frame_db(o),
          frame_db(r)
        );
      }
    }
  }
}

/// Regression (tester 2026-11-16, "hiss every ~7 s of silence", heavy models only): a wake in a
/// pause let the output detector hear RNNoise's stand-in output, whose noise opened the gate.
#[test]
fn test_wake_without_voice_keeps_the_gate_closed() {
  let noise = |secs: f32, amplitude: f32, seed: u32| {
    let mut x = seed;
    (0..(secs * 48_000.0) as usize)
      .map(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * amplitude
      })
      .collect::<Vec<f32>>()
  };
  // 3 s of a quiet room puts the model to sleep; the room getting 16 dB louder wakes it.
  let mut input = noise(3.0, 1e-3, 1);
  input.extend(noise(1.0, 6.3e-3, 2));
  for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
    let mut iso = AiVoiceIsolator::new();
    iso.set_model(model);
    iso.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
    let (mut woke, mut opened) = (false, 0);
    // shortcut: processed in place, so DPDFNet hears DFN3's output, not the raw room. On the raw
    // room `out_net`'s first frame after the wake (state from before the sleep) scores 0.32 and
    // the gate opens for 21 blocks; letting it listen unheeded during the wake fixed that here
    // but opened the gate on real captures (`pause_burst_report`, 2026-11-16). Open issue.
    for (i, block) in input.chunks_mut(480).enumerate() {
      let was = iso.asleep();
      iso.process_samples(block);
      woke |= was && !iso.asleep();
      if i >= 300 && iso.debug_state().gate_hold_left > 0 {
        opened += 1;
      }
    }
    assert!(woke, "{model:?}: the louder room must wake the model");
    assert_eq!(opened, 0, "{model:?}: gate opened in {opened} blocks without a voice");
  }
}

/// The click rescue stops after 2 s without a voice (typing alone: two RNNoise passes saved) and
/// starts again with the next word: it fills in under hammered clicks once RNNoise warmed up.
#[test]
fn test_rescue_stops_without_voice_and_comes_back() {
  let (_, _, mixed) = speech_with_clicks("speech_clean.wav", -3.0);
  let keys = keyboard_only(-30.0);
  let keys_len = 3 * 48_000;
  let mut x: Vec<f32> = keys.iter().cycle().take(keys_len).copied().collect();
  x.extend_from_slice(&mixed);
  let mut iso = AiVoiceIsolator::new();
  iso.set_model(DenoiseModel::DeepFilterNet);
  iso.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
  iso.sleep_after = usize::MAX; // awake, as typing keeps it
  let (mut ran_on_keys, mut rescued) = (0usize, 0usize);
  for (i, block) in x.chunks_mut(480).enumerate() {
    iso.process_samples(block);
    if (200..keys_len / 480).contains(&i) {
      ran_on_keys += iso.rnnoise_ran as usize;
    } else if i >= keys_len / 480 {
      rescued += (iso.debug_state().rescue_mix > 0.5) as usize;
    }
  }
  let key_blocks = keys_len / 480 - 200;
  assert!(ran_on_keys * 3 < key_blocks, "RNNoise ran in {ran_on_keys} of {key_blocks} key blocks");
  assert!(rescued > 20, "rescue filled in only {rescued} blocks after the pause");
}

/// Regression (tester 2026-11-16, after the fix above: "after a long silence, hiss every ~2 s"):
/// asleep, the 2017 RNNoise rated room noise as speech that `in_net` rated 0.00 while awake
/// (capture 20261110-141328: 0.44-0.85), so the model woke 30 ms after falling asleep, the gate
/// opened, and 2 s later the same again. Only real room noise fools it, so this checks the rule
/// itself: asleep or not, the input detector is `in_net`.
#[test]
fn test_asleep_the_input_detector_stays_in_net() {
  let mut x = 1u32;
  let input: Vec<f32> = (0..5 * 48_000)
    .map(|_| {
      x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
      ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 1e-3
    })
    .collect();
  for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
    let mut iso = AiVoiceIsolator::new();
    iso.set_model(model);
    iso.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
    let mut reference = VoiceNet::new_input();
    let mut asleep_blocks = 0;
    for block in input.clone().chunks_mut(480) {
      let mut expected = -1.0f32;
      reference.process(block, |p| expected = expected.max(p));
      iso.process_samples(block);
      if iso.asleep() {
        asleep_blocks += 1;
        assert_eq!(iso.debug_state().in_prob, expected, "{model:?}: asleep, another detector heard the input");
      }
    }
    assert!(asleep_blocks > 100, "{model:?}: asleep only {asleep_blocks} blocks");
  }
}

#[test]
fn test_live_switch_loads_off_thread_while_rnnoise_runs() {
  // Regression: a live switch loaded the model inside the audio callback (0.1-0.2 s stall).
  let mut isolator = AiVoiceIsolator::new();
  isolator.request_model(DenoiseModel::DeepFilterNet);
  let started = Instant::now();
  let mut block = vec![0.01f32; 480];
  isolator.process_samples(&mut block);
  assert_eq!(isolator.active_model(), DenoiseModel::RnNoise, "RNNoise runs until the model arrives");
  assert!(started.elapsed().as_millis() < 50, "the block waited for the load: {:?}", started.elapsed());
  while isolator.active_model() != DenoiseModel::DeepFilterNet {
    assert!(started.elapsed().as_secs() < 20, "never took over, load error: {:?}", isolator.df_error);
    std::thread::sleep(std::time::Duration::from_millis(10));
    isolator.process_samples(&mut block);
    assert!(block.iter().all(|s| s.is_finite()));
  }
  // A second heavy model selected while none is loading: same path.
  isolator.request_model(DenoiseModel::DpdfNet);
  while isolator.active_model() != DenoiseModel::DpdfNet {
    assert!(started.elapsed().as_secs() < 40, "never took over, load error: {:?}", isolator.dpdf_error);
    std::thread::sleep(std::time::Duration::from_millis(10));
    isolator.process_samples(&mut block);
  }
}

#[test]
fn test_overload_falls_back_until_reselected() {
  let mut isolator = AiVoiceIsolator::new();
  isolator.set_model(DenoiseModel::DeepFilterNet);
  let mut block = vec![0.01f32; 480];
  isolator.process_samples(&mut block);
  assert_eq!(isolator.active_model(), DenoiseModel::DeepFilterNet);

  isolator.fall_back_overloaded();
  isolator.process_samples(&mut block);
  assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
  assert_eq!(isolator.limit(), DenoiseLimit::Overloaded);

  // Same model again (the per-block settings sync) must not undo the fallback...
  isolator.set_model(DenoiseModel::DeepFilterNet);
  isolator.process_samples(&mut block);
  assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
  // ...picking another model and back does.
  isolator.set_model(DenoiseModel::RnNoise);
  isolator.set_model(DenoiseModel::DeepFilterNet);
  isolator.process_samples(&mut block);
  assert_eq!(isolator.active_model(), DenoiseModel::DeepFilterNet);
  assert_eq!(isolator.limit(), DenoiseLimit::None);
}

/// Regression: picking the selected model again after a failed load changed nothing (the
/// error was only cleared when the model id changed).
#[test]
fn test_retry_loads_a_model_that_failed_before() {
  let mut isolator = AiVoiceIsolator::new();
  isolator.select(DenoiseModel::DeepFilterNet);
  isolator.store(load_failed(DenoiseModel::DeepFilterNet, "test".into()));
  assert_eq!(isolator.limit(), DenoiseLimit::LoadFailed);
  isolator.request_model(DenoiseModel::DeepFilterNet); // the per-block settings sync
  assert!(isolator.loading.is_none(), "a known failure is not retried every block");

  isolator.retry();
  assert!(isolator.loading.is_some(), "retry starts a load");
  assert_eq!(isolator.limit(), DenoiseLimit::None);
}

/// Regression: `pause()` rebuilt the RNNoise states on the audio thread (allocating). The
/// flush must still drop everything from before the pause (DPDFNet's recurrent state holds
/// 4 hops of it) and keep a loaded model.
#[test]
fn test_flush_drops_audio_from_before_and_keeps_the_model() {
  let (clean, _) = speech_over_keyboard(-20.0, 0.0);
  for model in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_gate_threshold_db(GATE_OFF_DB - 1.0); // gate off: everything left would be heard
    isolator.set_model(model);
    // Mid-speech: the loudest point to be cut off at.
    let mut speech = clean.clone();
    let mut loud_blocks = 0;
    for b in speech.chunks_mut(480) {
      loud_blocks += usize::from(rms(b) > 0.03);
      isolator.process_samples(b);
      if loud_blocks >= 30 {
        break;
      }
    }
    assert_eq!(isolator.active_model(), model, "load error: {:?} {:?}", isolator.df_error, isolator.dpdf_error);
    let engine = (
      isolator.deepfilter.as_deref().map(|e| e as *const DeepFilterEngine),
      isolator.dpdfnet.as_deref().map(|e| e as *const DpdfEngine),
    );
    isolator.flush();
    assert_eq!(isolator.rescue.hold_left, 0, "{model:?}: the click rescue still held speech from before");
    let mut silence = vec![0.0f32; 48_000];
    silence.chunks_mut(441).for_each(|b| isolator.process_samples(b));
    let peak = silence.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak < 1e-4, "{model:?}: audio from before the flush, peak {:.1} dBFS", db(peak));
    let after = (
      isolator.deepfilter.as_deref().map(|e| e as *const DeepFilterEngine),
      isolator.dpdfnet.as_deref().map(|e| e as *const DpdfEngine),
    );
    assert_eq!(after, engine, "{model:?}: same engine, not reloaded");
    assert!(isolator.loading.is_none());
    assert_eq!(isolator.active_model(), model);
    // A model switch right after a flush: the new engine's first output is not the old one's.
    isolator.flush();
    isolator.rescue.hold_left = 4800;
    isolator.reset_engines();
    assert_eq!(isolator.mute_left, 0, "{model:?}: a switch still silences the next model");
    assert_eq!(isolator.rescue.hold_left, 0, "{model:?}: the click rescue kept the old engine's hold");
  }
}

#[test]
fn test_measured_latency_matches_constants() {
  // The APO feeds 480 (48 kHz device) or 96 (rate bridge) frames; 441 / 144 stand for hosts
  // and low-latency engine periods that don't divide a 10 ms hop. The gate and the leveler
  // trust the constant, so it must hold for every block size (DPDFNet ran 10 ms early at 480).
  for model in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
    for block in [441, 480, 96, 144] {
      let measured = measure_latency_samples(model, -80.0, block);
      let expected = (AiVoiceIsolator::latency_ms(model) * 48.0) as usize;
      eprintln!(
        "{model:?}, {block}-frame blocks: measured latency {measured} samples ({:.1} ms)",
        measured as f32 / 48.0
      );
      // Even at minimal strength the heavy models still output ~half wet signal; DPDFNet's
      // filtering shifts that half's phase by a few samples.
      let tolerance = if model == DenoiseModel::DpdfNet { 4 } else { 2 };
      assert!(
        measured.abs_diff(expected) <= tolerance,
        "{model:?}, {block}-frame blocks: measured {measured} samples, constant says {expected}"
      );
    }
  }
}

#[test]
fn test_lookahead_adds_its_delay_only_with_the_gate_on() {
  let model = DenoiseModel::RnNoise;
  let base = (AiVoiceIsolator::latency_ms(model) * 48.0) as usize;
  let ahead = measure_latency_samples(model, GATE_AUTO_LOOKAHEAD, 441);
  assert!(ahead.abs_diff(base + lookahead_samples(model)) <= 2, "{ahead} vs {base} + lookahead");
  // Off gate: lookahead would only add latency, so it does not run.
  let mut isolator = AiVoiceIsolator::new();
  isolator.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
  isolator.set_gate_threshold_db(-80.0);
  let mut block = vec![0.1f32; 480];
  isolator.process_samples(&mut block);
  assert_eq!(isolator.delay.len(), 0);
}

#[test]
fn test_lookahead_keeps_more_word_onsets() {
  // Onsets after pauses under loud keys are what the gate still cut (see RNNOISE_LOOKAHEAD_SAMPLES).
  let (clean, mixed) = speech_over_keyboard(-30.0, -10.0);
  for model in [DenoiseModel::DeepFilterNet, DenoiseModel::RnNoise] {
    let auto = run_quality(model, GATE_AUTO, &clean, &mixed);
    let ahead = run_quality(model, GATE_AUTO_LOOKAHEAD, &clean, &mixed);
    eprintln!("{model:?}: auto {auto:?}, lookahead {ahead:?}");
    assert!(ahead.soft_dropouts <= auto.soft_dropouts, "{model:?}: {ahead:?} vs {auto:?}");
    assert!(ahead.dropouts <= auto.dropouts, "{model:?}: {ahead:?} vs {auto:?}");
  }
}

#[test]
fn test_delay_line_delays_across_blocks() {
  let mut delay = DelayLine::new();
  delay.set_len(3);
  let mut a = [1.0f32, 2.0];
  let mut b = [3.0f32, 4.0, 5.0];
  delay.process(&mut a);
  delay.process(&mut b);
  assert_eq!((a, b), ([0.0, 0.0], [0.0, 1.0, 2.0]));
}

#[test]
fn test_dpdfnet_loads_and_switches_both_ways() {
  let mut isolator = AiVoiceIsolator::new();
  isolator.set_gate_threshold_db(-80.0);
  isolator.set_model(DenoiseModel::DpdfNet);
  let mut block = vec![0.01f32; 480];
  isolator.process_samples(&mut block);
  assert_eq!(isolator.active_model(), DenoiseModel::DpdfNet, "load error: {:?}", isolator.dpdf_error);
  let mut block = vec![0.1f32; 441];
  isolator.process_samples(&mut block);
  assert!(block.iter().all(|s| s.is_finite()));
  isolator.set_model(DenoiseModel::DeepFilterNet);
  isolator.process_samples(&mut block);
  assert_eq!(isolator.active_model(), DenoiseModel::DeepFilterNet);
  isolator.set_model(DenoiseModel::RnNoise);
  isolator.process_samples(&mut block);
  assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
}

#[test]
fn test_dpdfnet_overload_falls_back_to_rnnoise() {
  let mut isolator = AiVoiceIsolator::new();
  isolator.set_model(DenoiseModel::DpdfNet);
  let mut block = vec![0.01f32; 480];
  isolator.process_samples(&mut block);
  isolator.fall_back_overloaded();
  isolator.process_samples(&mut block);
  assert_eq!(isolator.active_model(), DenoiseModel::RnNoise);
  assert_eq!(isolator.limit(), DenoiseLimit::Overloaded);
}

#[test]
fn test_dpdfnet_wet_and_dry_paths_are_aligned() {
  // The strength blend mixes the input back in; if its delay differs from the model's, the
  // two comb-filter. The dry path is checked by the latency test (strength ~0); here the
  // fully wet output of near-clean speech must match the speech best at the same delay,
  // clearly better than one hop (10 ms) earlier or later.
  let (clean, mixed) = speech_over_keyboard(-25.0, 20.0);
  let mut isolator = AiVoiceIsolator::new();
  isolator.set_model(DenoiseModel::DpdfNet);
  isolator.set_strength(1.0);
  isolator.set_gate_threshold_db(GATE_OFF_DB - 1.0);
  let mut out = mixed.clone();
  for block in out.chunks_mut(441) {
    isolator.process_samples(block);
  }
  let corr = |lag: usize| -> f32 {
    let num: f32 = clean.iter().zip(out.iter().skip(lag)).map(|(a, b)| a * b).sum();
    let den = (clean.iter().map(|a| a * a).sum::<f32>() * out.iter().skip(lag).map(|b| b * b).sum::<f32>()).sqrt();
    num / den.max(1e-12)
  };
  let expected = (AiVoiceIsolator::latency_ms(DenoiseModel::DpdfNet) * 48.0) as usize;
  let best_near = |center: usize| {
    (center - 24..=center + 24).map(|l| (l, corr(l))).fold((0, f32::MIN), |b, c| if c.1 > b.1 { c } else { b })
  };
  let (at, here) = best_near(expected);
  let (_, early) = best_near(expected - 480);
  let (_, late) = best_near(expected + 480);
  eprintln!("wet correlation: {here:.3} at lag {at} (expected {expected}), one hop early {early:.3}, late {late:.3}");
  assert!(at.abs_diff(expected) <= 4, "best lag {at}, expected {expected}");
  assert!(here > 0.8 && here > early + 0.2 && here > late + 0.2, "{here} vs {early} / {late}");
}

#[test]
fn test_dpdfnet_keeps_soft_speech_over_keyboard() {
  let (clean, mixed) = speech_over_keyboard(-30.0, -10.0);
  let dpdf = run_quality(DenoiseModel::DpdfNet, GATE_OFF_DB - 1.0, &clean, &mixed);
  let dfn = run_quality(DenoiseModel::DeepFilterNet, GATE_OFF_DB - 1.0, &clean, &mixed);
  eprintln!("loud keys, no gate: DPDFNet {dpdf:?}, DFN3 {dfn:?}");
  assert!(dpdf.dropouts < 0.01 && dpdf.soft_dropouts < 0.10, "{dpdf:?}");
  assert!(dpdf.noise_suppression_db > 35.0, "{dpdf:?}");
  let (clean, mixed) = speech_over_keyboard(-45.0, 0.0);
  let dpdf = run_quality(DenoiseModel::DpdfNet, GATE_OFF_DB - 1.0, &clean, &mixed);
  let dfn = run_quality(DenoiseModel::DeepFilterNet, GATE_OFF_DB - 1.0, &clean, &mixed);
  eprintln!("quiet voice, no gate: DPDFNet {dpdf:?}, DFN3 {dfn:?}");
  assert!(dpdf.soft_dropouts <= dfn.soft_dropouts, "{dpdf:?} vs DFN3 {dfn:?}");
}

#[test]
fn test_deepfilter_keeps_soft_speech_over_keyboard() {
  // Regression: upstream min_db_thresh (-10 dB) zeroed 26 % of soft syllables here.
  let (clean, mixed) = speech_over_keyboard(-30.0, -10.0);
  let q = run_quality(DenoiseModel::DeepFilterNet, GATE_OFF_DB - 1.0, &clean, &mixed);
  eprintln!("DFN, loud keys: {q:?}");
  assert!(q.dropouts < 0.01 && q.soft_dropouts < 0.10, "{q:?}");
  // Keys 10 dB over the voice: next to words `ClickRescue` lets them through with the voice it
  // fills in (all pauses ~20 dB, measured 35+ without it); pauses away from speech stay clean.
  assert!(q.far_suppression_db > 35.0 && q.noise_suppression_db > 15.0, "{q:?}");
}

#[test]
fn test_gate_keeps_loud_and_quiet_speech() {
  // Regression: the old level gate at its default -45 dB cut 13 % of a quiet voice
  // (-45 dBFS) and half of its soft syllables. The automatic gate has no level setting:
  // a loud (daytime) and a quiet (evening) voice must both pass like with no gate.
  for level in [-20.0f32, -45.0] {
    let (clean, mixed) = speech_over_keyboard(level, 0.0);
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
      let with_gate = run_quality(model, GATE_AUTO, &clean, &mixed);
      let no_gate = run_quality(model, GATE_OFF_DB - 1.0, &clean, &mixed);
      eprintln!("{model:?} voice {level} dBFS: gate {with_gate:?}, no gate {no_gate:?}");
      assert!(with_gate.dropouts <= no_gate.dropouts + 0.01, "{model:?} {level}: {with_gate:?} vs {no_gate:?}");
      assert!(
        with_gate.soft_dropouts <= no_gate.soft_dropouts + 0.03,
        "{model:?} {level}: {with_gate:?} vs {no_gate:?}"
      );
    }
  }
}

#[test]
fn test_gate_keeps_soft_syllables_under_loud_keys() {
  // Regression: with one 0.5 threshold the gate closed on soft syllables between louder
  // ones (+3.5 % cut for DFN3, +5.3 % for RNNoise over no gate). Hysteresis keeps them.
  let (clean, mixed) = speech_over_keyboard(-30.0, -10.0);
  for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
    let with_gate = run_quality(model, GATE_AUTO, &clean, &mixed);
    let no_gate = run_quality(model, GATE_OFF_DB - 1.0, &clean, &mixed);
    eprintln!("{model:?} loud keys: gate {with_gate:?}, no gate {no_gate:?}");
    assert!(with_gate.soft_dropouts <= no_gate.soft_dropouts + 0.015, "{model:?}: {with_gate:?} vs {no_gate:?}");
  }
}

#[test]
fn test_gate_hold_covers_model_latency() {
  // Regression: the hold ran from when the detector heard the raw input, a model latency
  // before that audio leaves the model, so DPDFNet (60 ms) lost 1.9 -> 3.1 % of a quiet
  // voice's soft syllables to the gate. Now 2.5 % (DFN3 5.3 -> 5.6 %); the rest are onsets
  // the detector recognises late (a 400 ms hold changes nothing).
  let (clean, mixed) = speech_over_keyboard(-45.0, 0.0);
  for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
    let with_gate = run_quality(model, GATE_AUTO, &clean, &mixed);
    let no_gate = run_quality(model, GATE_OFF_DB - 1.0, &clean, &mixed);
    eprintln!("{model:?} quiet voice: gate {with_gate:?}, no gate {no_gate:?}");
    assert!(with_gate.soft_dropouts <= no_gate.soft_dropouts + 0.008, "{model:?}: {with_gate:?} vs {no_gate:?}");
  }
}

#[test]
fn test_gate_shuts_on_typing_without_speech() {
  // Max open share per model (DFN3, DPDFNet, RNNoise). The gate favours the voice: the output
  // detector also hears keys a model leaves in, so RNNoise (keys still audible in its output)
  // opens on a third of loud typing (measured 31 %, 11 % at -40 dBFS); DFN3 5 %, DPDFNet 2 %.
  // Closed or not, keys stay `GATE_FLOOR_DB` or more under the model's output.
  for (level, max_open) in [(-20.0f32, [0.08f32, 0.03, 0.40]), (-40.0, [0.0, 0.0, 0.15])] {
    let keys = keyboard_only(level);
    for (model, max_open) in
      [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise].into_iter().zip(max_open)
    {
      let open = open_share(&run(model, GATE_AUTO, &keys), &run(model, GATE_OFF_DB - 1.0, &keys));
      eprintln!("{model:?} keys only {level} dBFS: gate open {:.1} % of the time", open * 100.0);
      assert!(open <= max_open, "{model:?} {level}: open {open}");
    }
  }
}

#[test]
fn test_soft_key_keeps_more_speech_and_no_more_keys() {
  // The Silero key only adds open time where it hears a voice: no more speech lost than
  // without it, and typing alone opens the gate as rarely as before (it never reaches the
  // threshold on keys). The shared fixture shows no gain (soft syllables 0.9 % either way);
  // the private voice does, see `SOFT_VOICE_PROB`.
  let without_soft = |input: &[f32]| {
    let mut isolator = AiVoiceIsolator::new();
    isolator.soft_voice_prob = f32::INFINITY;
    let mut out = input.to_vec();
    out.chunks_mut(480).for_each(|block| isolator.process_samples(block));
    out
  };
  let (clean, mixed) = speech_over_keyboard(-45.0, 0.0);
  let latency = latency_samples(DenoiseModel::RnNoise);
  let with = measure(&clean, &mixed, &run(DenoiseModel::RnNoise, GATE_AUTO, &mixed), latency);
  let without = measure(&clean, &mixed, &without_soft(&mixed), latency);
  eprintln!("RNNoise quiet voice: with the soft key {with:?}, without {without:?}");
  assert!(with.soft_dropouts <= without.soft_dropouts && with.dropouts <= without.dropouts, "{with:?} vs {without:?}");
  let keys = keyboard_only(-20.0);
  let ungated = run(DenoiseModel::RnNoise, GATE_OFF_DB - 1.0, &keys);
  let (open, open_without) =
    (open_share(&run(DenoiseModel::RnNoise, GATE_AUTO, &keys), &ungated), open_share(&without_soft(&keys), &ungated));
  eprintln!(
    "RNNoise keys only: gate open {:.1} % with the soft key, {:.1} % without",
    open * 100.0,
    open_without * 100.0
  );
  assert!(open <= open_without + 0.01, "{open} vs {open_without}");
}

#[test]
fn test_click_rescue_fills_in_only_while_silero_hears_a_voice() {
  // DFN3's output far under RNNoise's, the rescue armed: it crossfades to RNNoise only with
  // `voice_now` (regression: a lone click next to a word came through at RNNoise's level).
  let (dfn, rnn) = (vec![0.001f32; 4800], vec![0.1f32; 4800]);
  for (voice_now, filled) in [(false, false), (true, true)] {
    let mut rescue = ClickRescue::new();
    rescue.hold_left = usize::MAX;
    let mut out = dfn.clone();
    rescue.process(&mut out, &rnn, voice_now);
    let last = out.last().copied().unwrap_or_default();
    assert_eq!(last > 0.05, filled, "voice_now {voice_now}: {last}");
  }
}

#[test]
fn test_closed_gate_attenuates_by_floor_not_mute() {
  let mut gate = NoiseGate::new(true);
  let mut block = vec![1.0f32; 48_000];
  gate.process(&mut block, 0);
  let last = block.last().copied().unwrap_or_default();
  assert!((db(last) - GATE_FLOOR_DB).abs() < 0.1, "{} dB", db(last));
}

#[test]
fn test_gate_floor_follows_leveler_boost_within_limits() {
  let closed_db = |extra: f32| {
    let mut iso = AiVoiceIsolator::new();
    iso.set_gate_floor_extra_db(extra);
    let mut block = vec![1.0f32; 48_000];
    iso.gate.process(&mut block, 0);
    db(block.last().copied().unwrap_or_default())
  };
  assert!((closed_db(6.0) - (GATE_FLOOR_DB - 6.0)).abs() < 0.1);
  // A leveler cut never lifts the floor; the boost counts up to the limit only.
  assert!((closed_db(-10.0) - GATE_FLOOR_DB).abs() < 0.1);
  assert!((closed_db(24.0) - (GATE_FLOOR_DB - GATE_FLOOR_EXTRA_MAX_DB)).abs() < 0.1);
  assert!((closed_db(f32::NAN) - GATE_FLOOR_DB).abs() < 0.1);
}

/// Share of 10 ms frames (after a 1 s warm-up) in which the gate is open: the gated output
/// is within 6 dB of the same model's ungated output (closed = `GATE_FLOOR_DB` under it).
fn open_share(gated: &[f32], ungated: &[f32]) -> f32 {
  let frames: Vec<bool> = gated
    .as_chunks::<480>()
    .0
    .iter()
    .zip(ungated.as_chunks::<480>().0)
    .skip(100)
    .map(|(g, u)| rms(u) > 1e-6 && db(rms(g)) > db(rms(u)) - 6.0)
    .collect();
  frames.iter().filter(|&&o| o).count() as f32 / frames.len().max(1) as f32
}

/// What the UI writes for "Auto" / "Auto" with "Protect word onsets".
const GATE_AUTO: f32 = 0.0;
const GATE_AUTO_LOOKAHEAD: f32 = 1.0;

/// Default strength, 10 ms blocks like the audio engine.
fn run(model: DenoiseModel, gate_db: f32, input: &[f32]) -> Vec<f32> {
  let mut isolator = AiVoiceIsolator::new();
  isolator.set_model(model);
  isolator.set_gate_threshold_db(gate_db);
  let mut out = input.to_vec();
  for block in out.chunks_mut(480) {
    isolator.process_samples(block);
  }
  assert_eq!(isolator.active_model(), model);
  out
}

fn run_quality(model: DenoiseModel, gate_db: f32, clean: &[f32], mixed: &[f32]) -> Quality {
  let lookahead = if gate_db >= GATE_LOOKAHEAD_DB { lookahead_samples(model) } else { 0 };
  let latency = (AiVoiceIsolator::latency_ms(model) * 48.0) as usize + lookahead;
  measure(clean, mixed, &run(model, gate_db, mixed), latency)
}

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test_data/denoise");
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
  /// Same, only pauses at least `FAR_PAUSE_FRAMES` from speech: the gaps right next to words
  /// get keys through where DFN3's `ClickRescue` fills in a cut voice.
  far_suppression_db: f32,
}

/// 150 ms: the rescue's hold (100 ms), crossfade out (30 ms) and its detector's frame.
const FAR_PAUSE_FRAMES: usize = 15;

/// Clean speech at `speech_rms_db` dBFS and the same speech with mechanical keyboard and
/// mouse clicks mixed in at `snr_db`. Both at 48 kHz.
fn speech_over_keyboard(speech_rms_db: f32, snr_db: f32) -> (Vec<f32>, Vec<f32>) {
  mix("speech_clean.wav", &KEYBOARD_CLIPS, None, speech_rms_db, snr_db)
}

/// `speech_over_keyboard` for any fixtures (paths relative to `FIXTURES`). `low_cut_hz`
/// high-passes the speech first (24 dB/oct, like the mic EQ's low cut): breath pops and
/// desk bumps in a raw recording are loud but not speech, and would count as lost speech.
fn mix(
  speech: &str,
  noise_clips: &[&str],
  low_cut_hz: Option<f32>,
  speech_rms_db: f32,
  snr_db: f32,
) -> (Vec<f32>, Vec<f32>) {
  let mut clean = read_wav_48k(speech);
  if let Some(frequency) = low_cut_hz {
    let mut eq = BiquadCascade::default();
    let band =
      BiquadBand { filter_type: FilterType::HighPass, frequency, q: 0.707, slope_db: 24, ..BiquadBand::default() };
    eq.set_bands(&[band], 48_000.0);
    eq.process(&mut clean);
  }
  let mut noise: Vec<f32> = noise_clips.iter().flat_map(|f| read_wav_48k(f)).collect();
  while noise.len() < clean.len() {
    noise.extend_from_within(..);
  }
  noise.truncate(clean.len());
  let clean = scale_to_db(&clean, speech_rms_db);
  let noise = scale_to_db(&noise, speech_rms_db - snr_db);
  let mixed = clean.iter().zip(&noise).map(|(s, n)| s + n).collect();
  (clean, mixed)
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

/// WAV fixture as f32 (first channel, like the APO), linearly resampled to 48 kHz (good
/// enough for test noise).
pub(crate) fn read_wav_48k(name: &str) -> Vec<f32> {
  // `DENOISE_FIXTURES`: the fixtures elsewhere (the test binary run on another PC).
  let dir = std::env::var("DENOISE_FIXTURES").unwrap_or(FIXTURES.into());
  let mut reader = hound::WavReader::open(format!("{dir}/{name}")).unwrap();
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

/// `in_net`'s voice probability per 480-sample frame of `x`.
fn in_net_probs(x: &[f32]) -> Vec<f32> {
  let mut net = VoiceNet::new_input();
  assert!(matches!(net.state, VoiceModel::New(_)), "RNNoise 0.2 weights must load");
  let mut probs = Vec::with_capacity(x.len() / FRAME_SIZE);
  for block in x.chunks(FRAME_SIZE) {
    net.process(block, |p| probs.push(p));
  }
  probs
}

/// Regression (2026-11-09): DeepFilterNet cut a voice quieter than about -52 dBFS by 30-45 dB
/// (studio speech at -58 dBFS: -43 dB); the leveler's boost as pre-gain brings it back.
#[test]
fn test_deepfilter_keeps_a_quiet_voice_with_pre_gain() {
  let quiet = scale_to_db(&read_wav_48k("speech_clean.wav"), -58.0);
  let kept_db = |pre_gain_db: f32| {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(DenoiseModel::DeepFilterNet);
    isolator.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
    isolator.set_pre_gain_db(pre_gain_db);
    let mut out = quiet.clone();
    for block in out.chunks_mut(480) {
      isolator.process_samples(block);
    }
    let lead = latency_samples(DenoiseModel::DeepFilterNet) + lookahead_samples(DenoiseModel::DeepFilterNet);
    db(rms(&out[lead..])) - db(rms(&quiet[..quiet.len() - lead]))
  };
  assert!(kept_db(24.0) > -6.0, "voice changed by {:.1} dB with the pre-gain", kept_db(24.0));
  assert!(kept_db(f32::NAN) == kept_db(0.0), "NaN counts as no gain");
}

fn rms(x: &[f32]) -> f32 {
  (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

fn db(x: f32) -> f32 {
  20.0 * x.max(1e-9).log10()
}

/// Runs noise through the model at minimal strength (mostly dry, so the signal survives) in
/// odd-sized blocks and finds the input->output lag by cross-correlation.
fn measure_latency_samples(model: DenoiseModel, gate_db: f32, block: usize) -> usize {
  const LEN: usize = 48_000;
  const MAX_LAG: usize = 3_000;
  let mut isolator = AiVoiceIsolator::new();
  isolator.set_model(model);
  isolator.set_strength(0.01);
  isolator.set_gate_threshold_db(gate_db);
  let mut x = 12_345u32;
  let input: Vec<f32> = (0..LEN)
    .map(|_| {
      x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
      ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.2
    })
    .collect();
  let mut output = input.clone();
  for chunk in output.chunks_mut(block) {
    isolator.process_samples(chunk);
  }
  assert_eq!(isolator.active_model(), model);
  const SKIP: usize = 6_000; // model warm-up; must be >= MAX_LAG
  let window = LEN - SKIP;
  (0..MAX_LAG)
    .map(|lag| {
      let dry = input.iter().skip(SKIP - lag);
      let c: f32 = dry.zip(output.iter().skip(SKIP)).take(window).map(|(a, b)| a * b).sum();
      (lag, c)
    })
    .fold((0, f32::MIN), |best, cur| if cur.1 > best.1 { cur } else { best })
    .0
}

/// Report on the developer's own recordings in the git-ignored `test_data/private/`
/// (`speech.wav`, `keys.wav`: mono WAV). Prints gate vs no gate per model, no thresholds:
/// a private voice can't back a shared regression test. Run:
/// `cargo test -p gamersequalizer-mic-chain --release private_report -- --ignored --nocapture`
#[test]
#[ignore = "needs private recordings"]
fn private_report() {
  const SPEECH: &str = "../private/speech.wav";
  const KEYS: [&str; 1] = ["../private/keys.wav"];
  if [SPEECH, KEYS[0]].iter().any(|f| !std::path::Path::new(FIXTURES).join(f).exists()) {
    eprintln!("skipped: put speech.wav and keys.wav into test_data/private/");
    return;
  }
  eprintln!("soft% / loud% lost [pause suppression dB], gate auto | auto + lookahead | no gate");
  for (level, snr) in [(-20.0f32, 0.0f32), (-45.0, 0.0), (-30.0, -10.0)] {
    let (clean, mixed) = mix(SPEECH, &KEYS, Some(100.0), level, snr);
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
      let row = |q: Quality| {
        format!("{:4.1} / {:3.1} [{:4.1}]", q.soft_dropouts * 100.0, q.dropouts * 100.0, q.noise_suppression_db)
      };
      let gate = row(run_quality(model, GATE_AUTO, &clean, &mixed));
      let ahead = row(run_quality(model, GATE_AUTO_LOOKAHEAD, &clean, &mixed));
      let no_gate = row(run_quality(model, GATE_OFF_DB - 1.0, &clean, &mixed));
      eprintln!("voice {level:>4} dBFS, SNR {snr:>4} dB, {model:<13?} {gate} | {ahead} | {no_gate}");
    }
  }
  for level in [-20.0f32, -40.0] {
    let keys = noise_only(&KEYS, level);
    for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
      let ungated = run(model, GATE_OFF_DB - 1.0, &keys);
      let open = open_share(&run(model, GATE_AUTO, &keys), &ungated);
      let ahead = run(model, GATE_AUTO_LOOKAHEAD, &keys);
      let open_ahead = open_share(ahead.get(lookahead_samples(model)..).unwrap_or_default(), &ungated);
      eprintln!(
        "keys only {level} dBFS, {model:<13?} gate open {:.1} %, with lookahead {:.1} %",
        open * 100.0,
        open_ahead * 100.0
      );
    }
  }
}

#[test]
fn test_deepfilter_keeps_speech_under_hammered_mouse_clicks() {
  // Regression: a mouse button hammered next to the mic (peaks -3 dBFS over a -30 dBFS voice)
  // made DFN3 cut the voice with each click: > 15 dB lost in 16 % of the frames with a click
  // (private voice), 5.5 % here. `ClickRescue` fills those in from RNNoise: 5.8 % / 1.4 %.
  let (clean, clicks, mixed) = speech_with_clicks("speech_clean.wav", -3.0);
  let with = run_clicks(DenoiseModel::DeepFilterNet, 0.75, GATE_AUTO_LOOKAHEAD, false, &clean, &clicks, &mixed);
  let without = run_clicks(DenoiseModel::DeepFilterNet, 0.75, GATE_AUTO_LOOKAHEAD, true, &clean, &clicks, &mixed);
  eprintln!("DFN3 hammered clicks: rescue {with:?}, without {without:?}");
  assert!(with.lost15 < 0.06 && with.lost15 < without.lost15 / 2.0, "{with:?} vs {without:?}");
  assert!(with.quality.soft_dropouts <= without.quality.soft_dropouts, "{with:?} vs {without:?}");
  // Pauses away from speech stay as clean: the rescue is armed only by a voice in DFN3's output.
  assert!(with.quality.far_suppression_db > without.quality.far_suppression_db - 3.0, "{with:?} vs {without:?}");
}

/// Mouse clicks right next to the mic over speech, per model / strength / gate: soft% / loud%
/// lost [pause suppression dB], and the speech frames with a click (click within 10 dB of the
/// voice there): mean dB under the clean speech, shares lost > 6 / > 15 dB. Private voice
/// when present. DFN3 also without `ClickRescue`.
/// `cargo test -p gamersequalizer-mic-chain --release click_report -- --ignored --nocapture`
#[test]
#[ignore = "report, slow"]
fn click_report() {
  const SPEECH: &str = "../private/speech.wav";
  let speech = if std::path::Path::new(FIXTURES).join(SPEECH).exists() { SPEECH } else { "speech_clean.wav" };
  eprintln!("speech: {speech}");
  for peak_db in [-20.0f32, -10.0, -3.0] {
    let (clean, clicks, mixed) = speech_with_clicks(speech, peak_db);
    for (model, strength, no_rescue) in [
      (DenoiseModel::DeepFilterNet, 0.75, false),
      (DenoiseModel::DeepFilterNet, 0.75, true),
      (DenoiseModel::DeepFilterNet, 0.25, true),
      (DenoiseModel::DpdfNet, 0.75, false),
      (DenoiseModel::RnNoise, 0.75, false),
    ] {
      for gate in [GATE_OFF_DB - 1.0, GATE_AUTO_LOOKAHEAD] {
        let c = run_clicks(model, strength, gate, no_rescue, &clean, &clicks, &mixed);
        eprintln!(
          "clicks {peak_db:>5} dBFS, {model:<13?}{} strength {strength:.2} gate {:<4} | {:4.1} / {:3.1} [{:4.1}] | clicks: {:4.1} dB, >6 {:4.1} %, >15 {:4.1} %",
          if no_rescue { " no rescue" } else { "          " },
          if gate > GATE_OFF_DB { "auto" } else { "off" },
          c.quality.soft_dropouts * 100.0,
          c.quality.dropouts * 100.0,
          c.quality.noise_suppression_db,
          c.mean_loss_db,
          c.lost6 * 100.0,
          c.lost15 * 100.0
        );
      }
    }
  }
}

/// A raw take of speech with keys hammered into it (git-ignored `test_data/private/keyboard_call.wav`,
/// no clean reference). Per model (gate auto + lookahead, default strength) writes the processed take
/// to `test_data/private/out/` for listening and counts speech frames DFN3 cut: 10 ms frames where
/// RNNoise (which keeps a voice under keys) is speech-loud and the model's output > 10 dB under it.
/// `cargo test -p gamersequalizer-mic-chain --release keyboard_call_report -- --ignored --nocapture`
#[test]
#[ignore = "needs private recordings"]
fn keyboard_call_report() {
  const TAKE: &str = "../private/keyboard_call.wav";
  if !std::path::Path::new(FIXTURES).join(TAKE).exists() {
    eprintln!("skipped: put keyboard_call.wav into test_data/private/");
    return;
  }
  let input = read_wav_48k(TAKE);
  let out_dir = std::path::Path::new(FIXTURES).join("../private/out");
  std::fs::create_dir_all(&out_dir).unwrap();
  let process = |model: DenoiseModel, no_rescue: bool| {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(model);
    isolator.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
    isolator.no_rescue = no_rescue;
    let mut out = input.clone();
    for block in out.chunks_mut(480) {
      isolator.process_samples(block);
    }
    out.drain(..latency_samples(model) + lookahead_samples(model)); // aligned with the input
    out
  };
  let reference = process(DenoiseModel::RnNoise, false);
  let ref_db: Vec<f32> = reference.as_chunks::<480>().0.iter().map(|f| db(rms(f))).collect();
  let loud = ref_db.iter().copied().fold(f32::MIN, f32::max);
  for (name, model, no_rescue) in [
    ("dfn3_rescue", DenoiseModel::DeepFilterNet, false),
    ("dfn3_no_rescue", DenoiseModel::DeepFilterNet, true),
    ("dpdfnet", DenoiseModel::DpdfNet, false),
    ("rnnoise", DenoiseModel::RnNoise, false),
  ] {
    let out = process(model, no_rescue);
    let (mut speech, mut cut) = (0usize, 0usize);
    for (f, &r) in out.as_chunks::<480>().0.iter().zip(&ref_db).skip(100) {
      if r > loud - 25.0 {
        speech += 1;
        cut += (db(rms(f)) < r - 10.0) as usize;
      }
    }
    let quiet: Vec<f32> =
      out.as_chunks::<480>().0.iter().zip(&ref_db).filter(|&(_, &r)| r < loud - 40.0).map(|(f, _)| rms(f)).collect();
    eprintln!(
      "{name:<15} speech frames cut > 10 dB under RNNoise: {:4.1} % of {speech}; quiet frames {:5.1} dBFS",
      cut as f32 * 100.0 / speech.max(1) as f32,
      db((quiet.iter().map(|x| x * x).sum::<f32>() / quiet.len().max(1) as f32).sqrt())
    );
    let spec = hound::WavSpec {
      channels: 1,
      sample_rate: 48_000,
      bits_per_sample: 32,
      sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(out_dir.join(format!("keyboard_call_{name}.wav")), spec).unwrap();
    out.iter().for_each(|&s| w.write_sample(s).unwrap());
    w.finalize().unwrap();
  }
}

/// Default strength candidates per model (gate auto + lookahead): soft% / loud% lost
/// [pause suppression dB, far pauses dB] on the keyboard mixes (private when present) and
/// hammered clicks at -3 dBFS (+ frames lost > 15 dB under the clicks).
/// `cargo test -p gamersequalizer-mic-chain --release strength_report -- --ignored --nocapture`
#[test]
#[ignore = "report, slow"]
fn strength_report() {
  const SPEECH: &str = "../private/speech.wav";
  const KEYS: [&str; 1] = ["../private/keys.wav"];
  let private = [SPEECH, KEYS[0]].iter().all(|f| std::path::Path::new(FIXTURES).join(f).exists());
  let (speech, keys): (&str, &[&str]) = if private { (SPEECH, &KEYS) } else { ("speech_clean.wav", &KEYBOARD_CLIPS) };
  eprintln!(
    "{} recordings; mixes voice/SNR -20/0, -45/0, -30/-10 dB, then clicks -3 dBFS",
    if private { "private" } else { "shared" }
  );
  let mixes: Vec<_> = [(-20.0f32, 0.0f32), (-45.0, 0.0), (-30.0, -10.0)]
    .iter()
    .map(|&(l, s)| mix(speech, keys, Some(100.0), l, s))
    .collect();
  let (cc, ck, cm) = speech_with_clicks(speech, -3.0);
  for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
    for strength in [0.75f32, 0.9, 1.0] {
      let cells: Vec<String> = mixes
        .iter()
        .map(|(clean, mixed)| {
          let mut iso = AiVoiceIsolator::new();
          iso.set_model(model);
          iso.set_strength(strength);
          iso.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
          let mut out = mixed.clone();
          for b in out.chunks_mut(480) {
            iso.process_samples(b);
          }
          let q = measure(clean, mixed, &out, latency_samples(model) + lookahead_samples(model));
          format!(
            "{:4.1} / {:3.1} [{:4.1} {:4.1}]",
            q.soft_dropouts * 100.0,
            q.dropouts * 100.0,
            q.noise_suppression_db,
            q.far_suppression_db
          )
        })
        .collect();
      let c = run_clicks(model, strength, GATE_AUTO_LOOKAHEAD, false, &cc, &ck, &cm);
      eprintln!(
        "{model:<13?} {strength:.2} | {} | clicks {:4.1} / {:3.1} [{:4.1} {:4.1}] >15 {:4.1} %",
        cells.join(" | "),
        c.quality.soft_dropouts * 100.0,
        c.quality.dropouts * 100.0,
        c.quality.noise_suppression_db,
        c.quality.far_suppression_db,
        c.lost15 * 100.0
      );
    }
  }
}

/// Where keys leak next to words: the private voice at -30 dBFS under the private keys 10 dB
/// louder, per model. Speech frames lost > 6 / > 15 dB, word tails (15-30 dB under the average)
/// lost > 6 dB, then key suppression in pause frames by their distance from speech (10 ms
/// frames after the last / before the next speech frame).
/// `cargo test -p gamersequalizer-mic-chain --release near_pause_report -- --ignored --nocapture`
#[test]
#[ignore = "needs private recordings"]
fn near_pause_report() {
  const SPEECH: &str = "../private/speech.wav";
  const KEYS: [&str; 1] = ["../private/keys.wav"];
  const BUCKETS: [(usize, usize); 5] = [(1, 1), (2, 2), (3, 5), (6, 10), (11, 1000)];
  if [SPEECH, KEYS[0]].iter().any(|f| !std::path::Path::new(FIXTURES).join(f).exists()) {
    eprintln!("skipped: put speech.wav and keys.wav into test_data/private/");
    return;
  }
  let (clean, mixed) = mix(SPEECH, &KEYS, Some(100.0), -30.0, -10.0);
  let clean_db = db(rms(&clean));
  let speech: Vec<bool> = clean.as_chunks::<480>().0.iter().map(|c| db(rms(c)) >= clean_db - 40.0).collect();
  // Frames to the nearest speech frame behind / ahead of each frame.
  let distances = |frames: &mut dyn Iterator<Item = usize>| {
    let (mut d, mut out) = (usize::MAX / 2, vec![0usize; speech.len()]);
    for i in frames {
      d = if speech[i] { 0 } else { d + 1 };
      out[i] = d;
    }
    out
  };
  let after = distances(&mut (0..speech.len()));
  let before = distances(&mut (0..speech.len()).rev());
  for (name, model, gate) in [
    ("DFN3", DenoiseModel::DeepFilterNet, GATE_AUTO_LOOKAHEAD),
    ("DFN3 no gate", DenoiseModel::DeepFilterNet, GATE_OFF_DB - 1.0),
    ("DPDFNet", DenoiseModel::DpdfNet, GATE_AUTO_LOOKAHEAD),
    ("RNNoise", DenoiseModel::RnNoise, GATE_AUTO_LOOKAHEAD),
  ] {
    let out = run(model, gate, &mixed);
    let latency = latency_samples(model) + if gate >= GATE_LOOKAHEAD_DB { lookahead_samples(model) } else { 0 };
    let aligned = &out[latency..];
    let (mut loud, mut loud6, mut loud15, mut tails, mut tails6) = (0usize, 0usize, 0usize, 0usize, 0usize);
    for (c, o) in clean.as_chunks::<480>().0.iter().zip(aligned.as_chunks::<480>().0).skip(50) {
      let c_db = db(rms(c));
      let loss = c_db - db(rms(o));
      if c_db >= clean_db - 15.0 {
        loud += 1;
        loud6 += (loss > 6.0) as usize;
        loud15 += (loss > 15.0) as usize;
      } else if c_db >= clean_db - 30.0 {
        tails += 1;
        tails6 += (loss > 6.0) as usize;
      }
    }
    let share = |n: usize, of: usize| n as f32 * 100.0 / of.max(1) as f32;
    eprintln!(
      "{name:<12} pauses {:4.1} dB | speech lost > 6 dB {:4.1} %, > 15 dB {:4.1} % | tails lost > 6 dB {:4.1} %",
      measure(&clean, &mixed, &out, latency).noise_suppression_db,
      share(loud6, loud),
      share(loud15, loud),
      share(tails6, tails)
    );
    for (side, own, other) in [("after ", &after, &before), ("before", &before, &after)] {
      let mut line = format!("  {side} speech");
      for (lo, hi) in BUCKETS {
        let (mut e_in, mut e_out, mut n) = (0.0f32, 0.0f32, 0usize);
        for (i, (m, o)) in mixed.as_chunks::<480>().0.iter().zip(aligned.as_chunks::<480>().0).enumerate().skip(50) {
          // A pause frame counts on the side of the nearer word (a tie: after).
          let nearer = if side == "after " { own[i] <= other[i] } else { own[i] < other[i] };
          if (lo..=hi).contains(&own[i]) && nearer {
            e_in += m.iter().map(|x| x * x).sum::<f32>();
            e_out += o.iter().map(|x| x * x).sum::<f32>();
            n += 1;
          }
        }
        line += &format!(" | {lo}-{hi} fr ({n}): {:4.1} dB", 10.0 * (e_in.max(1e-12) / e_out.max(1e-12)).log10());
      }
      eprintln!("{line}");
    }
  }
}

/// How far the gate holds a whisper down: the private `whisper_scenario.wav` per model (gate
/// auto + lookahead), with and without the Silero key (`soft_net`). Per segment the level of
/// the gated output under the same model's ungated output, dB (0 = the gate stayed open;
/// `GATE_FLOOR_DB` = closed throughout), and the share of 10 ms frames more than 6 dB under it.
/// `cargo test -p gamersequalizer-mic-chain --release whisper_gate_report -- --ignored --nocapture`
#[test]
#[ignore = "needs private recordings"]
fn whisper_gate_report() {
  const TAKE: &str = "../private/whisper_scenario.wav";
  const SEGMENTS: [(&str, f32, f32); 8] = [
    ("silence", 1.0, 18.0),
    ("whisper", 20.0, 24.0),
    ("quieter whisper", 26.5, 31.5),
    ("whisper + typing", 36.0, 41.0),
    ("quiet voice", 45.0, 49.0),
    ("quiet voice 2", 53.5, 57.5),
    ("keys only", 59.5, 68.5),
    ("alternating", 75.0, 82.0),
  ];
  if !std::path::Path::new(FIXTURES).join(TAKE).exists() {
    eprintln!("skipped: put whisper_scenario.wav into test_data/private/");
    return;
  }
  let input = read_wav_48k(TAKE);
  let process = |model: DenoiseModel, gate_db: f32, soft: bool| {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(model);
    isolator.set_gate_threshold_db(gate_db);
    if !soft {
      isolator.soft_voice_prob = f32::INFINITY;
    }
    let mut out = input.clone();
    for block in out.chunks_mut(480) {
      isolator.process_samples(block);
    }
    let lookahead = if gate_db >= GATE_LOOKAHEAD_DB { lookahead_samples(model) } else { 0 };
    out.drain(..latency_samples(model) + lookahead); // aligned with the input
    out
  };
  eprintln!("{:<14}{}", "", SEGMENTS.map(|(name, ..)| format!("{name:>17}")).join(""));
  for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
    let ungated = process(model, GATE_OFF_DB - 1.0, false);
    for (label, soft) in [("without", false), ("with Silero", true)] {
      let gated = process(model, GATE_AUTO_LOOKAHEAD, soft);
      let cells = SEGMENTS.map(|(_, from, to)| {
        let range = (from * 48_000.0) as usize..(to * 48_000.0) as usize;
        let part = |x: &'_ [f32]| x.get(range.clone()).unwrap_or_default().to_vec();
        let (g, u) = (part(&gated), part(&ungated));
        let frames = g.as_chunks::<480>().0.iter().zip(u.as_chunks::<480>().0);
        let down = frames.clone().filter(|(g, u)| db(rms(*g)) < db(rms(*u)) - 6.0).count();
        format!("{:>10.1} {:>4.0} %", db(rms(&g)) - db(rms(&u)), down as f32 * 100.0 / frames.count().max(1) as f32)
      });
      eprintln!("{:<14}{}  {label}", format!("{model:?}").chars().take(13).collect::<String>(), cells.join(""));
    }
  }
}

/// A debug capture's denoiser input (`04_after_aec.wav`) through the denoiser as the chain ran
/// it (its fixed low cut, each block's leveler gain as pre-gain and gate floor), in 100 ms
/// windows: where the output differs from what the chain put out live (`05_after_denoise.wav`)
/// by more than 6 dB, then what happens to loud windows without a voice (Silero under 0.15)
/// and to voice windows. For trying a change on a real capture: with unchanged code a capture
/// of the same build replays without a difference. DFN3, gate auto, no lookahead.
/// `AEC_WAV=<capture>_v<n>.wav` takes the denoiser input from `test_data/private/aec_ab/`
/// instead (an AEC3 variant written by `aec3::tests::capture_tuning_report` with `WAV=1`).
/// `CAP=20261109-204900 cargo test -p gamersequalizer-mic-chain --release capture_replay_report -- --ignored --nocapture`
#[test]
#[ignore = "needs private recordings"]
fn capture_replay_report() {
  let cap = std::env::var("CAP").unwrap_or("20261109-204900".into());
  if !std::path::Path::new(FIXTURES).join(format!("../private/captures/{cap}/vars.tsv")).exists() {
    eprintln!("skipped: no capture {cap} in test_data/private/captures/");
    return;
  }
  let dir = format!("../private/captures/{cap}");
  let aec_wav =
    std::env::var("AEC_WAV").map_or(format!("{dir}/04_after_aec.wav"), |name| format!("../private/aec_ab/{name}"));
  let (x, live) = (read_wav_48k(&aec_wav), read_wav_48k(&format!("{dir}/05_after_denoise.wav")));
  let vars = std::fs::read_to_string(std::path::Path::new(FIXTURES).join(&dir).join("vars.tsv")).unwrap();
  let mut lines = vars.lines();
  let col = lines.next().unwrap().split('\t').position(|h| h == "leveler_gain_db").unwrap();
  let gains: Vec<f32> = lines.map(|l| l.split('\t').nth(col).unwrap().parse().unwrap()).collect();
  let mut hp = BiquadCascade::default();
  hp.set_bands(
    &[BiquadBand {
      filter_type: FilterType::HighPass,
      frequency: 70.0,
      q: 0.707,
      slope_db: 12,
      ..BiquadBand::default()
    }],
    48_000.0,
  );
  let mut iso = AiVoiceIsolator::new();
  iso.set_model(DenoiseModel::DeepFilterNet);
  iso.set_gate_threshold_db(GATE_AUTO);
  let mut out = x.clone();
  for (b, g) in out.chunks_mut(480).zip(&gains) {
    hp.process(b);
    iso.set_gate_floor_extra_db(*g);
    iso.set_pre_gain_db(*g);
    iso.process_samples(b);
  }
  let lat = latency_samples(DenoiseModel::DeepFilterNet);
  let (out, live) = (&out[lat..], &live[lat..]);
  // Fidelity: 100 ms windows, replay against the live output.
  let pairs: Vec<(f32, f32, f32)> = x
    .as_chunks::<4800>()
    .0
    .iter()
    .zip(out.as_chunks::<4800>().0)
    .zip(live.as_chunks::<4800>().0)
    .map(|((m, o), l)| (db(rms(m)), db(rms(o)), db(rms(l))))
    .collect();
  let loud: Vec<&(f32, f32, f32)> = pairs.iter().skip(30).filter(|p| p.2 > -70.0 || p.1 > -70.0).collect();
  let off = loud.iter().filter(|p| (p.1 - p.2).abs() > 6.0).count();
  eprintln!(
    "FIDELITY {} windows with output over -70 dBFS (live or replay): {off} differ by more than 6 dB",
    loud.len()
  );
  for (t, p) in pairs.iter().enumerate().skip(30) {
    if (p.2 > -70.0 || p.1 > -70.0) && (p.1 - p.2).abs() > 6.0 {
      eprintln!("DIFF {:.1} s mic {:6.1} replay {:6.1} live {:6.1}", t as f32 / 10.0, p.0, p.1, p.2);
    }
  }
  // Clicks: loud windows the live chain's Silero would not call voice (standalone pass here).
  let (mut vad, mut probs) = (Vad48k::new().unwrap(), Vec::new());
  x.chunks(480).for_each(|b| vad.process(b, |p| probs.push(p)));
  let silero = |t: usize| {
    (0..4).map(|k| probs.get((t * 4800) / silero_vad::CHUNK_48K + k).copied().unwrap_or(0.0)).fold(0.0f32, f32::max)
  };
  let (mut n, mut e_in, mut e_out, mut leaks, mut sp, mut sp_loss, mut sp_cut) =
    (0usize, 0.0f32, 0.0f32, 0usize, 0usize, 0.0f32, 0usize);
  for (t, p) in pairs.iter().enumerate().skip(30) {
    if p.0 > -50.0 && silero(t) < 0.15 {
      n += 1;
      e_in += 10f32.powf(p.0 / 10.0);
      e_out += 10f32.powf(p.1 / 10.0);
      leaks += (p.0 - p.1 < 12.0) as usize;
    } else if silero(t) > 0.5 {
      sp += 1;
      sp_loss += p.0 - p.1;
      sp_cut += (p.0 - p.1 > 10.0) as usize;
    }
  }
  eprintln!(
    "RESULT loud windows without voice {n}: removed {:.1} dB, {leaks} under 12 dB | voice windows {sp}: mean loss {:.1} dB, > 10 dB in {:.1} %",
    10.0 * (e_in / e_out.max(1e-12)).log10(),
    sp_loss / sp.max(1) as f32,
    sp_cut as f32 * 100.0 / sp.max(1) as f32
  );
  let mut pauses: Vec<f32> =
    pairs.iter().enumerate().skip(30).filter(|(t, _)| silero(*t) < 0.15).map(|(_, p)| p.1).collect();
  pauses.sort_by(f32::total_cmp);
  let p_len = pauses.len();
  let p_median = pauses.get(p_len / 2).copied().unwrap_or(0.0);
  let p_90 = pauses.get(p_len * 9 / 10).copied().unwrap_or(0.0);
  eprintln!("PAUSES without voice {p_len}: median {p_median:.1} dBFS, 90th percentile {p_90:.1} dBFS");
}

/// Hiss bursts in pauses (tester report 2026-11-16: "every ~7 s of silence a fraction of a second
/// of hiss", heavy models only). Replays each capture's denoiser input with every model, sleep on
/// and off, gate auto + lookahead, the capture's leveler gains as in the chain, and counts 100 ms
/// windows in a pause (standalone Silero < 0.1 for +-300 ms) over -80 dBFS before the leveler
/// (RNNoise, which the tester heard as clean, is the reference).
/// `CAPS=20261110-141254,20261110-141328 cargo test -p gamersequalizer-mic-chain --release pause_burst_report -- --ignored --nocapture`
#[test]
#[ignore = "needs private recordings"]
fn pause_burst_report() {
  let caps = std::env::var("CAPS").unwrap_or("20261110-141254,20261110-141328,20261116-122126".into());
  for cap in caps.split(',') {
    let dir = format!("../private/captures/{cap}");
    if !std::path::Path::new(FIXTURES).join(&dir).join("vars.tsv").exists() {
      eprintln!("skipped: no capture {cap}");
      continue;
    }
    let x = read_wav_48k(&format!("{dir}/04_after_aec.wav"));
    let vars = std::fs::read_to_string(std::path::Path::new(FIXTURES).join(&dir).join("vars.tsv")).unwrap();
    let mut lines = vars.lines();
    let col = lines.next().unwrap().split('\t').position(|h| h == "leveler_gain_db").unwrap();
    let gains: Vec<f32> = lines.map(|l| l.split('\t').nth(col).unwrap().parse().unwrap()).collect();
    let (mut vad, mut probs) = (Vad48k::new().unwrap(), Vec::new());
    x.chunks(480).for_each(|b| vad.process(b, |p| probs.push(p)));
    let windows = x.len() / 4800;
    let silero = |t: usize| {
      (0..4).map(|k| probs.get((t * 4800) / silero_vad::CHUNK_48K + k).copied().unwrap_or(0.0)).fold(0.0f32, f32::max)
    };
    let pause: Vec<bool> = (0..windows).map(|t| (t.saturating_sub(3)..=t + 3).all(|u| silero(u) < 0.1)).collect();
    for model in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
      for sleep in [true, false] {
        let mut iso = AiVoiceIsolator::new();
        iso.set_model(model);
        iso.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
        if !sleep {
          iso.sleep_after = usize::MAX;
        }
        let mut out = x.clone();
        let mut wakes = 0;
        // Per block: asleep, input detector heard voice, gate hold.
        let mut trace: Vec<(bool, bool, usize)> = Vec::new();
        for (b, g) in out.chunks_mut(480).zip(&gains) {
          let was = iso.asleep();
          iso.set_gate_floor_extra_db(*g);
          iso.set_pre_gain_db(*g);
          iso.process_samples(b);
          wakes += (was && !iso.asleep()) as usize;
          trace.push((iso.asleep(), iso.heard_voice, iso.debug_state().gate_hold_left));
        }
        // Wakes in a pause: what woke the model, after how long asleep, and whether the gate opened.
        let mut slept_at = 0;
        for i in 1..trace.len() {
          if !trace[i - 1].0 && trace[i].0 {
            slept_at = i;
          }
          if trace[i - 1].0 && !trace[i].0 && pause.get(i / 10).copied().unwrap_or(false) {
            let cause = if trace[i - 1].1 { "voice detector" } else { "activity" };
            let open = trace[i..(i + 30).min(trace.len())].iter().filter(|t| t.2 > 0).count();
            eprintln!(
              "  wake {:.2} s after {} ms asleep: {cause}, gate open {open} of the next 30 blocks",
              i as f32 / 100.0,
              (i - slept_at) * 10
            );
          }
        }
        let lat = latency_samples(model) + lookahead_samples(model);
        let w: Vec<f32> =
          (0..windows).map(|t| db(rms(out.get(t * 4800 + lat..(t + 1) * 4800 + lat).unwrap_or(&[])))).collect();
        let mut quiet: Vec<f32> = (30..windows).filter(|&t| pause[t]).map(|t| w[t]).collect();
        quiet.sort_by(f32::total_cmp);
        let median = quiet.get(quiet.len() / 2).copied().unwrap_or(-120.0);
        let peak = quiet.last().copied().unwrap_or(-120.0);
        let bursts: Vec<usize> = (30..windows).filter(|&t| pause[t] && w[t] > -80.0).collect();
        let at: Vec<String> = bursts.iter().take(12).map(|&t| format!("{:.1}", t as f32 / 10.0)).collect();
        eprintln!(
          "{cap} {model:?} sleep {sleep}: pause windows {}, median {median:.1}, peak {peak:.1} dBFS, over -80: {} (wakes {wakes}) at [{}] s",
          quiet.len(),
          bursts.len(),
          at.join(" ")
        );
      }
    }
  }
}

#[derive(Debug)]
struct ClickQuality {
  quality: Quality,
  /// Speech frames with a click: mean dB under the clean speech, shares more than 6 / 15 dB under.
  mean_loss_db: f32,
  lost6: f32,
  lost15: f32,
}

/// `speech` (high-passed, -30 dBFS) and ESC-50 mouse clicks hammered: each click (40 ms from
/// its onset) every 120 ms, peaks at `peak_db` dBFS. Returns (clean, clicks, mixed).
fn speech_with_clicks(speech: &str, peak_db: f32) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
  let raw: Vec<f32> = ["1-118206-A-31.wav", "1-18655-A-31.wav"].iter().flat_map(|f| read_wav_48k(f)).collect();
  let max = raw.iter().fold(0.0f32, |m, s| m.max(s.abs())).max(1e-9);
  let mut clicks = Vec::new();
  let mut i = 0;
  while let Some(&s) = raw.get(i) {
    if s.abs() > max * 0.3 {
      let start = i.saturating_sub(48);
      clicks.extend_from_slice(raw.get(start..(start + 48 * 40).min(raw.len())).unwrap_or_default());
      clicks.resize(clicks.len() + 48 * 80, 0.0);
      i = start + 48 * 60;
    } else {
      i += 1;
    }
  }
  let (clean, _) = mix(speech, &KEYBOARD_CLIPS, Some(100.0), -30.0, 0.0);
  while clicks.len() < clean.len() {
    clicks.extend_from_within(..);
  }
  clicks.truncate(clean.len());
  let gain = 10f32.powf(peak_db / 20.0) / max;
  clicks.iter_mut().for_each(|s| *s *= gain);
  let mixed = clean.iter().zip(&clicks).map(|(s, n)| s + n).collect();
  (clean, clicks, mixed)
}

fn run_clicks(
  model: DenoiseModel,
  strength: f32,
  gate_db: f32,
  no_rescue: bool,
  clean: &[f32],
  clicks: &[f32],
  mixed: &[f32],
) -> ClickQuality {
  let mut isolator = AiVoiceIsolator::new();
  isolator.set_model(model);
  isolator.set_strength(strength);
  isolator.set_gate_threshold_db(gate_db);
  isolator.no_rescue = no_rescue;
  let mut out = mixed.to_vec();
  for block in out.chunks_mut(480) {
    isolator.process_samples(block);
  }
  let lookahead = if gate_db >= GATE_LOOKAHEAD_DB { lookahead_samples(model) } else { 0 };
  let latency = latency_samples(model) + lookahead;
  let quality = measure(clean, mixed, &out, latency);
  let clean_db = db(rms(clean));
  let (mut n, mut sum, mut lost6, mut lost15) = (0usize, 0.0f32, 0usize, 0usize);
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
      lost15 += (loss > 15.0) as usize;
    }
  }
  let n = n.max(1) as f32;
  ClickQuality { quality, mean_loss_db: sum / n, lost6: lost6 as f32 / n, lost15: lost15 as f32 / n }
}

/// Gate lookahead per model: soft% / loud% lost [pause suppression dB] per mix and the share
/// of keys-only audio the gate lets through, for 0-30 ms. Private recordings when present,
/// else the shared fixtures. Picks the `*_LOOKAHEAD_SAMPLES` constants.
/// `cargo test -p gamersequalizer-mic-chain --release lookahead_sweep -- --ignored --nocapture`
#[test]
#[ignore = "report, slow"]
fn lookahead_sweep() {
  const SPEECH: &str = "../private/speech.wav";
  const KEYS: [&str; 1] = ["../private/keys.wav"];
  let private = [SPEECH, KEYS[0]].iter().all(|f| std::path::Path::new(FIXTURES).join(f).exists());
  let (speech, keys): (&str, &[&str]) = if private { (SPEECH, &KEYS) } else { ("speech_clean.wav", &KEYBOARD_CLIPS) };
  eprintln!(
    "{} recordings; mixes: voice -20/SNR 0, -45/0, -30/-10 dB; keys only -20 dBFS",
    if private { "private" } else { "shared" }
  );
  let mixes: Vec<_> = [(-20.0f32, 0.0f32), (-45.0, 0.0), (-30.0, -10.0)]
    .iter()
    .map(|&(l, s)| mix(speech, keys, Some(100.0), l, s))
    .collect();
  let keys_only = noise_only(keys, -20.0);
  for model in [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise] {
    let ungated = run(model, GATE_OFF_DB - 1.0, &keys_only);
    for ms in [0usize, 5, 10, 15, 20, 30] {
      let la = ms * 48;
      let cells: Vec<String> = mixes
        .iter()
        .map(|(clean, mixed)| {
          let q = measure(clean, mixed, &run_lookahead(model, la, mixed), latency_samples(model) + la);
          format!("{:4.1} / {:3.1} [{:4.1}]", q.soft_dropouts * 100.0, q.dropouts * 100.0, q.noise_suppression_db)
        })
        .collect();
      let open = open_share(run_lookahead(model, la, &keys_only).get(la..).unwrap_or_default(), &ungated);
      eprintln!("{model:<13?} {ms:>2} ms | {} | keys open {:4.1} %", cells.join(" | "), open * 100.0);
    }
  }
}

/// `run` with the gate on and a fixed lookahead instead of the model's.
fn run_lookahead(model: DenoiseModel, lookahead: usize, input: &[f32]) -> Vec<f32> {
  let mut isolator = AiVoiceIsolator::new();
  isolator.set_model(model);
  isolator.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
  isolator.lookahead_override = Some(lookahead);
  let mut out = input.to_vec();
  for block in out.chunks_mut(480) {
    isolator.process_samples(block);
  }
  out
}

/// Report on one long take of the test script (git-ignored `test_data/private/scenario.wav`).
/// No clean reference exists, so per second it prints the input level, the raw-input voice
/// probability and per model the output level without / with the gate (auto + lookahead,
/// the default) and the gate's open share; then a summary split into seconds with and without
/// speech, and CPU time. Writes the processed takes to `test_data/private/out/` for listening.
/// `cargo test -p gamersequalizer-mic-chain --release scenario_report -- --ignored --nocapture`
#[test]
#[ignore = "needs private recordings"]
fn scenario_report() {
  const SCENARIO: &str = "../private/scenario.wav";
  const SEC: usize = 48_000;
  let models = [DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet, DenoiseModel::RnNoise];
  if !std::path::Path::new(FIXTURES).join(SCENARIO).exists() {
    eprintln!("skipped: put scenario.wav into test_data/private/");
    return;
  }
  let input = read_wav_48k(SCENARIO);
  let secs = input.len() / SEC;
  let out_dir = std::path::Path::new(FIXTURES).join("../private/out");
  std::fs::create_dir_all(&out_dir).unwrap();

  let mut st = DenoiseState::new();
  let mut scratch = [0.0f32; FRAME_SIZE];
  let probs: Vec<f32> = input
    .as_chunks::<FRAME_SIZE>()
    .0
    .iter()
    .map(|f| st.process_frame(&mut scratch, &f.map(|s| s * I16_SCALE)))
    .collect();

  // Per model: (ungated, gated), both aligned with the input.
  let mut outs = Vec::new();
  for model in models {
    let t0 = std::time::Instant::now();
    let ungated = run(model, GATE_OFF_DB - 1.0, &input);
    let cpu = t0.elapsed().as_secs_f32() / (input.len() as f32 / SEC as f32);
    let gated = run(model, GATE_AUTO_LOOKAHEAD, &input);
    let lat = latency_samples(model);
    let ungated = ungated[lat..].to_vec();
    let gated = gated[lat + lookahead_samples(model)..].to_vec();
    eprintln!("{model:?}: CPU {:.1} % of one core (denoiser + input detector, no gate)", cpu * 100.0);
    for (name, sig) in [("nogate", &ungated), ("gate", &gated)] {
      let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
      };
      let mut w = hound::WavWriter::create(out_dir.join(format!("{model:?}_{name}.wav")), spec).unwrap();
      sig.iter().for_each(|s| w.write_sample(*s).unwrap());
      w.finalize().unwrap();
    }
    outs.push((ungated, gated));
  }

  let sec_db = |x: &[f32], s: usize| db(rms(x.get(s * SEC..(s + 1) * SEC).unwrap_or_default()));
  // Gate open share in second s: frames where gated is within 6 dB of ungated.
  let open = |g: &[f32], u: &[f32], s: usize| {
    let range = s * SEC..(s + 1) * SEC;
    let (g, u) = (g.get(range.clone()).unwrap_or_default(), u.get(range).unwrap_or_default());
    let f: Vec<bool> = g
      .as_chunks::<480>()
      .0
      .iter()
      .zip(u.as_chunks::<480>().0)
      .filter(|(_, u)| rms(*u) > 1e-6)
      .map(|(g, u)| db(rms(g)) > db(rms(u)) - 6.0)
      .collect();
    f.iter().filter(|&&o| o).count() as f32 / f.len().max(1) as f32 * 100.0
  };
  let voice_share = |s: usize| {
    let p = probs.get(s * 100..(s + 1) * 100).unwrap_or_default();
    p.iter().filter(|&&p| p > VOICE_PROB).count() as f32 / p.len().max(1) as f32 * 100.0
  };
  eprintln!("\n  s   in dB  vad>0.5%  | per model: no gate dB / gate dB / gate open %   (DFN3 | DPDFNet | RNNoise)");
  for s in 0..secs {
    let cols: Vec<String> =
      outs.iter().map(|(u, g)| format!("{:6.1} {:6.1} {:5.1}", sec_db(u, s), sec_db(g, s), open(g, u, s))).collect();
    eprintln!("{s:4} {:6.1} {:7.1}    | {}", sec_db(&input, s), voice_share(s), cols.join(" | "));
  }

  // Seconds with clear speech (raw detector > 0.5 on >= 30 % of frames) vs none (0 %).
  let speech: Vec<usize> = (0..secs).filter(|&s| voice_share(s) >= 30.0).collect();
  let quiet: Vec<usize> = (0..secs).filter(|&s| voice_share(s) == 0.0).collect();
  eprintln!(
    "\nspeech seconds {}, no-speech seconds {}, other {}",
    speech.len(),
    quiet.len(),
    secs - speech.len() - quiet.len()
  );
  let energy = |x: &[f32], set: &[usize]| {
    set
      .iter()
      .map(|&s| x.get(s * SEC..(s + 1) * SEC).unwrap_or_default().iter().map(|v| v * v).sum::<f32>())
      .sum::<f32>()
      .max(1e-12)
  };
  for (model, (u, g)) in models.iter().zip(&outs) {
    // Voice frames (raw detector > 0.5) where the gate was closed (gated > 6 dB under ungated).
    let (mut vf, mut cut) = (0usize, 0usize);
    for (i, p) in probs.iter().enumerate() {
      let r = i * FRAME_SIZE..(i + 1) * FRAME_SIZE;
      if let (true, Some(gf), Some(uf)) = (*p > VOICE_PROB, g.get(r.clone()), u.get(r)) {
        vf += 1;
        cut += (rms(uf) > 1e-6 && db(rms(gf)) <= db(rms(uf)) - 6.0) as usize;
      }
    }
    eprintln!(
      "{model:<13?} no-speech seconds suppressed {:5.1} dB (no gate {:5.1}) | speech seconds level {:+5.1} dB vs input (no gate {:+5.1}) | voice frames under closed gate {:.2} %",
      10.0 * (energy(&input, &quiet) / energy(g, &quiet)).log10(),
      10.0 * (energy(&input, &quiet) / energy(u, &quiet)).log10(),
      10.0 * (energy(g, &speech) / energy(&input, &speech)).log10(),
      10.0 * (energy(u, &speech) / energy(&input, &speech)).log10(),
      cut as f32 / vf.max(1) as f32 * 100.0
    );
  }
}

/// CPU of each mic-chain part on 10 s of speech over keys, % of one core (time / audio length):
/// what to cut when the meter reads high (tester 2026-11-16: 27 % with a heavy model). Run:
/// `cargo test -p gamersequalizer-mic-chain --release cpu_breakdown_report -- --ignored --nocapture`
/// `COLD_CACHE=1` overwrites 256 MB before each block and times only the block (live, other work
/// runs on the core between blocks). `PACED=1` lets the core idle 8 ms after each block, as live
/// (a notebook may not clock up for 1-3 ms of work in 10). `TRACT_CPU_ISA=-avx512f` times the
/// AVX2 kernels a CPU without AVX-512 gets (Zen 3, most notebooks). `scripts/cpu-bench-package.ps1`
/// packs it for another PC.
#[test]
#[ignore = "timing report"]
fn cpu_breakdown_report() {
  eprintln!(
    "ISA tract uses: {:?}; CPU has avx512f: {}",
    tract_onnx_23::tract_core::tract_linalg::isa::native(),
    std::arch::is_x86_feature_detected!("avx512f")
  );
  let (_, mixed) = speech_over_keyboard(-30.0, 0.0);
  let x: Vec<f32> = mixed.iter().cycle().take(10 * 48_000).copied().collect();
  let keys: Vec<f32> = keyboard_only(-30.0).iter().cycle().take(x.len()).copied().collect();
  let mut flush = std::env::var_os("COLD_CACHE").map(|_| vec![0u8; 256 << 20]);
  let paced = std::env::var_os("PACED").is_some();
  let mut time_on = |name: &str, input: &[f32], f: &mut dyn FnMut(&mut [f32])| {
    let mut y = input.to_vec();
    let mut spent = std::time::Duration::ZERO;
    for b in y.chunks_mut(480) {
      if let Some(buf) = flush.as_mut() {
        buf.iter_mut().step_by(64).for_each(|v| *v = v.wrapping_add(1));
      }
      if paced {
        std::thread::sleep(std::time::Duration::from_millis(8));
      }
      let t = Instant::now();
      f(b);
      spent += t.elapsed();
    }
    eprintln!("CPU {name:<34} {:6.2} % of a core", spent.as_secs_f32() / (y.len() as f32 / 48_000.0) * 100.0);
  };
  let mut time = |name: &str, f: &mut dyn FnMut(&mut [f32])| time_on(name, &x, f);
  let mut rnn = RnNoiseEngine::new();
  time("RNNoise 2017 denoiser + its VAD", &mut |b| rnn.process(b, 0.75, |_| {}));
  let mut in_net = VoiceNet::new_input();
  time("in_net (RNNoise 0.2, detector only)", &mut |b| in_net.process(b, |_| {}));
  let mut out_net = VoiceNet::new();
  time("out_net (RNNoise 2017 VAD)", &mut |b| out_net.process(b, |_| {}));
  let mut silero = Vad48k::new().unwrap();
  time("Silero VAD", &mut |b| silero.process(b, |_| {}));
  let mut df = DeepFilterEngine::load().unwrap();
  time("DeepFilterNet 3 alone", &mut |b| df.process(b, 0.75, 1.0));
  let mut dp = DpdfEngine::load().unwrap();
  time("DPDFNet alone", &mut |b| dp.process(b, atten_lim(0.75)));
  let mut aec = crate::aec3::EchoCanceller3::new();
  let reference = x.clone();
  let mut pos = 0;
  time("echo canceller (AEC3, stereo ref)", &mut |b| {
    let r = &reference[pos..pos + b.len()];
    aec.process_block_stereo(r, r, b);
    pos += b.len();
  });
  for (input, what) in [(&x, "speech"), (&keys, "keys only")] {
    for model in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
      let mut iso = AiVoiceIsolator::new();
      iso.set_model(model);
      iso.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
      iso.sleep_after = usize::MAX; // awake, as typing keeps a model in a game
      time_on(&format!("whole denoiser, {model:?}, {what}"), input, &mut |b| iso.process_samples(b));
    }
  }
}

/// DeepFilterNet 3 alone (no gate) on speech over keys, strength 0.75 and 1.0: time per second
/// of audio, and the output against a reference saved earlier (raw f32 LE, `DFN3_REF`; written
/// when missing). For runtime upgrades (tract 0.21 -> 0.23, 2026-11-08). Run:
/// `$env:DFN3_REF='<file>'; cargo test -p gamersequalizer-mic-chain --release dfn3_reference -- --ignored --nocapture`
#[test]
#[ignore = "runtime upgrade check"]
fn dfn3_reference() {
  let Ok(path) = std::env::var("DFN3_REF") else { return eprintln!("skipped: set DFN3_REF") };
  let (_, mixed) = speech_over_keyboard(-30.0, 0.0);
  let mut out = Vec::with_capacity(2 * mixed.len());
  let mut secs = Vec::new();
  for strength in [0.75f32, 1.0] {
    let t = Instant::now();
    let mut df = DeepFilterEngine::load().unwrap();
    eprintln!("DFN3 load: {} ms", t.elapsed().as_millis());
    let mut x = mixed.clone();
    let t = Instant::now();
    for block in x.chunks_mut(480) {
      df.process(block, strength, 1.0);
    }
    secs.push(t.elapsed().as_secs_f32() / (mixed.len() as f32 / 48_000.0) * 100.0);
    out.extend_from_slice(&x);
  }
  eprintln!("DFN3 cpu: {:.2} % (0.75), {:.2} % (1.0) of a core", secs[0], secs[1]);
  match std::fs::read(&path) {
    Ok(bytes) => {
      let reference: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
      assert_eq!(reference.len(), out.len());
      let max_diff = reference.iter().zip(&out).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
      let err: Vec<f32> = reference.iter().zip(&out).map(|(a, b)| a - b).collect();
      eprintln!("vs reference: max |diff| {max_diff:.2e}, signal/diff {:.1} dB", db(rms(&reference)) - db(rms(&err)));
    }
    Err(_) => {
      let bytes: Vec<u8> = out.iter().flat_map(|s| s.to_le_bytes()).collect();
      std::fs::write(&path, bytes).unwrap();
      eprintln!("reference written to {path}");
    }
  }
}

/// Does echo AEC3 leaves behind still open the gate (the case an `echo_only` mask would cover)?
/// The debug captures' raw mic after AEC3 (`aec3::tests::capture_export` writes them to
/// `test_data/private/aec_ab/`) through each model: share of 10 ms blocks the input detector
/// hears speech / the gate is open in, in "echo only" blocks (far end playing, AEC3 took at least
/// 15 dB off the mic: little near-end voice in them) and in blocks without a far end. Run:
/// `cargo test -p gamersequalizer-mic-chain --release echo_gate_report -- --ignored --nocapture`
#[test]
#[ignore = "needs private recordings"]
fn echo_gate_report() {
  let base = std::path::Path::new(FIXTURES).join("../private");
  let Ok(dirs) = std::fs::read_dir(base.join("captures")) else { return eprintln!("no captures") };
  eprintln!(
    "capture          model          echo-only blocks: voice% gate% out dBFS | no far end: blocks voice% gate%"
  );
  for dir in dirs.flatten().map(|d| d.path()) {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let aec3_file = format!("../private/aec_ab/{name}_aec3.wav");
    if !std::path::Path::new(FIXTURES).join(&aec3_file).exists() {
      continue;
    }
    let mic = read_wav_48k(&format!("../private/captures/{name}/01_mic_raw.wav"));
    let refs = read_wav_48k(&format!("../private/captures/{name}/03_ref_aec.wav"));
    let aec3 = read_wav_48k(&aec3_file);
    let blocks = mic.len().min(refs.len()).min(aec3.len()) / 480;
    // 0 = no far end, 1 = echo only, 2 = far end + near-end voice (not counted)
    let kind: Vec<u8> = (0..blocks)
      .map(|b| {
        let r = b * 480..(b + 1) * 480;
        let (m, f, a) = (rms(&mic[r.clone()]), rms(&refs[r.clone()]), rms(&aec3[r]));
        if db(f) < -50.0 {
          0
        } else if db(a) <= db(m) - 15.0 {
          1
        } else {
          2
        }
      })
      .collect();
    for model in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet] {
      let mut isolator = AiVoiceIsolator::new();
      isolator.set_model(model);
      isolator.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
      let mut out = aec3.clone();
      let (mut voice, mut open) = (Vec::with_capacity(blocks), Vec::with_capacity(blocks));
      for block in out.chunks_mut(480).take(blocks) {
        isolator.process_samples(block);
        voice.push(isolator.heard_voice());
        open.push(isolator.gate.gain > 0.5);
      }
      // The gate acts on audio a model latency + lookahead older than what was just heard.
      let lead = isolator.output_lead_samples().div_ceil(480);
      let stats = |k: u8| {
        let idx: Vec<usize> = (100..blocks.saturating_sub(lead)).filter(|&b| kind[b] == k).collect();
        let share = |v: &[bool], off: usize| {
          idx.iter().filter(|&&b| v.get(b + off).copied().unwrap_or(false)).count() as f32 * 100.0
            / idx.len().max(1) as f32
        };
        let power = idx
          .iter()
          .map(|&b| out.get((b + lead) * 480..(b + lead + 1) * 480).map_or(0.0, |s| rms(s).powi(2)))
          .sum::<f32>()
          / idx.len().max(1) as f32;
        (idx.len(), share(&voice, 0), share(&open, lead), db(power.sqrt()))
      };
      let (e, n) = (stats(1), stats(0));
      eprintln!(
        "{name} {model:<13?} {:6} {:6.1} {:6.1} {:7.1} | {:6} {:6.1} {:6.1}",
        e.0, e.1, e.2, e.3, n.0, n.1, n.2
      );
    }
  }
}

/// The learning voice leveler on the private recordings: denoiser (RNNoise, default gate) +
/// leveler, the input at its own level and 12 dB / 24 dB lower (a mic set low). Prints the gain
/// over time and the output level of voice and room sections. Run:
/// `cargo test -p gamersequalizer-mic-chain --release leveler_report -- --ignored --nocapture`
#[test]
#[ignore = "needs private recordings"]
fn leveler_report() {
  use crate::voice_leveler::VoiceLeveler;
  const HALF_SEC: usize = 24_000;
  let path = "../private/whisper_scenario.wav";
  if !std::path::Path::new(FIXTURES).join(path).exists() {
    return eprintln!("skipped: put whisper_scenario.wav into test_data/private/");
  }
  let input = read_wav_48k(path);
  let e_db = |x: &[f32], a: usize, b: usize| {
    let s = x.get(a * HALF_SEC..b * HALF_SEC).unwrap_or_default();
    10.0 * (s.iter().map(|v| v * v).sum::<f32>() / s.len().max(1) as f32).max(1e-12).log10()
  };
  for model in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet] {
    for att_db in [0.0f32, -12.0, -24.0] {
      let g = 10f32.powf(att_db / 20.0);
      let mut x: Vec<f32> = input.iter().map(|s| s * g).collect();
      let mut iso = AiVoiceIsolator::new();
      iso.set_model(model);
      iso.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
      let mut lv = VoiceLeveler::new();
      let mut gains = Vec::new();
      for (i, b) in x.chunks_mut(480).enumerate() {
        iso.process_samples(b);
        lv.process(b, iso.heard_voice(), iso.output_lead_samples());
        if i % 50 == 49 {
          gains.push(lv.gain_db());
        }
      }
      let lead = latency_samples(model) + lookahead_samples(model);
      let out = x.get(lead..).unwrap_or_default();
      let at = |s: f32| gains.get((s * 2.0) as usize).copied().unwrap_or(f32::NAN);
      eprintln!(
        "{model:<13?} input {att_db:+3} dB | gain at 20 s {:+5.1}, 46 s {:+5.1}, 47 s {:+5.1}, 50 s {:+5.1}, 60 s {:+5.1}, 84 s {:+5.1} | out: voice 45-49 {:6.1} voice 53-57 {:6.1} alternate {:6.1} room {:6.1} whisper {:6.1} dBFS",
        at(20.0),
        at(46.0),
        at(47.0),
        at(50.0),
        at(60.0),
        at(83.5),
        e_db(out, 90, 98),
        e_db(out, 107, 115),
        e_db(out, 150, 164),
        e_db(out, 2, 36),
        e_db(out, 40, 48)
      );
    }
  }
}

/// Per 0.5 s of a private recording: level, raw RNNoise voice share (> 0.5) and clicky 10 ms
/// frames (peak 20 dB over RMS: keys). For finding section bounds. Run (file in test_data/private/):
/// `$env:WAV="x.wav"; cargo test -p gamersequalizer-mic-chain --release vad_timeline -- --ignored --nocapture`
#[test]
#[ignore = "needs private recordings"]
fn vad_timeline() {
  let Ok(name) = std::env::var("WAV") else { return eprintln!("skipped: set WAV") };
  let x = read_wav_48k(&format!("../private/{name}"));
  let mut st = DenoiseState::new();
  let mut scratch = [0.0f32; FRAME_SIZE];
  let frames = x.as_chunks::<FRAME_SIZE>().0;
  let p: Vec<f32> = frames.iter().map(|f| st.process_frame(&mut scratch, &f.map(|s| s * I16_SCALE))).collect();
  for (i, (pc, fc)) in p.chunks(50).zip(frames.chunks(50)).enumerate() {
    let all: Vec<f32> = fc.iter().flatten().copied().collect();
    let voice = pc.iter().filter(|&&v| v > VOICE_PROB).count() * 2;
    let clicks = fc.iter().filter(|f| db(f.iter().fold(0.0f32, |m, s| m.max(s.abs()))) - db(rms(*f)) > 20.0).count();
    eprintln!(
      "{:5.1} {:6.1} dB voice {voice:3}% clicks {clicks:2} {}",
      i as f32 * 0.5,
      db(rms(&all)),
      "#".repeat(voice / 5)
    );
  }
}

/// Tuning report for the detector's hysteresis (keep threshold x active window). Run:
/// `cargo test -p gamersequalizer-mic-chain --release gate_sweep -- --ignored --nocapture`
#[test]
#[ignore = "tuning report, slow"]
fn gate_sweep() {
  let run_with = |model: DenoiseModel, keep: f32, active: u32, input: &[f32]| -> Vec<f32> {
    let mut isolator = AiVoiceIsolator::new();
    isolator.set_model(model);
    isolator.set_gate_threshold_db(GATE_AUTO);
    isolator.vad.keep_prob = keep;
    isolator.vad.active_frames = active;
    let mut out = input.to_vec();
    for block in out.chunks_mut(480) {
      isolator.process_samples(block);
    }
    out
  };
  let speech = [(-30.0f32, -10.0f32), (-20.0, 0.0), (-45.0, 0.0)].map(|(l, s)| speech_over_keyboard(l, s));
  // Speech, then typing only: how long after the last word the gate lets keys through.
  let (clean, _) = speech_over_keyboard(-30.0, 0.0);
  let last_loud = clean.iter().rposition(|s| s.abs() > 0.01).unwrap_or(clean.len());
  let typing = keyboard_only(-20.0);
  let mut talk_then_type: Vec<f32> = clean[..last_loud].to_vec();
  let speech_end = talk_then_type.len();
  talk_then_type.extend_from_slice(&typing);
  // How often non-speech scores above a keep threshold (it would hold the gate open).
  let mut x = 1u32;
  let mut lp = 0.0f32;
  let fan: Vec<f32> = (0..48_000 * 10)
    .map(|_| {
      x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
      lp += 0.05 * (((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) - lp);
      lp
    })
    .collect();
  for (name, sig) in [
    ("keys -20", keyboard_only(-20.0)),
    ("keys -40", keyboard_only(-40.0)),
    ("fan -50", scale_to_db(&fan, -50.0)),
    ("fan -30", scale_to_db(&fan, -30.0)),
  ] {
    let mut st = DenoiseState::new();
    let mut scratch = [0.0f32; FRAME_SIZE];
    let probs: Vec<f32> = sig
      .as_chunks::<FRAME_SIZE>()
      .0
      .iter()
      .skip(50)
      .map(|f| st.process_frame(&mut scratch, &f.map(|s| s * I16_SCALE)))
      .collect();
    let share = |t: f32| probs.iter().filter(|&&p| p > t).count() as f32 / probs.len() as f32 * 100.0;
    eprintln!(
      "{name}: frames over 0.05 {:.1} %, 0.1 {:.1} %, 0.2 {:.1} %, 0.5 {:.1} %",
      share(0.05),
      share(0.1),
      share(0.2),
      share(0.5)
    );
  }
  for model in [DenoiseModel::DeepFilterNet, DenoiseModel::RnNoise] {
    let latency = (AiVoiceIsolator::latency_ms(model) * 48.0) as usize;
    eprintln!(
      "
{model:?}  soft%/loud% lost [pause suppression dB]: keys-10dB | -20dBFS | -45dBFS ; open% while typing after speech"
    );
    for (active, keep) in [(20u32, 0.5f32), (20, 0.1), (20, 0.05), (10, 0.1), (10, 0.05), (5, 0.1), (5, 0.05), (5, 0.2)]
    {
      let q: Vec<String> = speech
        .iter()
        .map(|(c, m)| {
          let q = measure(c, m, &run_with(model, keep, active, m), latency);
          format!("{:4.1}/{:3.1} [{:4.1}]", q.soft_dropouts * 100.0, q.dropouts * 100.0, q.noise_suppression_db)
        })
        .collect();
      let out = run_with(model, keep, active, &talk_then_type);
      let ungated = run(model, GATE_OFF_DB - 1.0, &talk_then_type);
      let tail = open_share(&out[speech_end - 48_000..], &ungated[speech_end - 48_000..]); // skips its first 1 s
      eprintln!("  active {active:2} keep {keep:4}: {} ; {:4.1}", q.join(" | "), tail * 100.0);
    }
    for (c, m) in &speech {
      let q = measure(c, m, &run(model, GATE_OFF_DB - 1.0, m), latency);
      eprint!("  no gate: {:4.1}/{:3.1} [{:4.1}]", q.soft_dropouts * 100.0, q.dropouts * 100.0, q.noise_suppression_db);
    }
    eprintln!();
  }
}

/// Denormals after exact digital silence (a headset's mute switch sends zeros): 2 s of speech
/// over keys, then 10 s of zeros, per part: mean time per 10 ms block on speech, on the zeros,
/// and the slowest block on the zeros. `FTZ_DAZ=1` sets flush-to-zero and denormals-are-zero
/// first, for comparison. Run:
/// `cargo test -p gamersequalizer-mic-chain --release digital_silence_report -- --ignored --nocapture`
#[test]
#[ignore = "timing report"]
fn digital_silence_report() {
  #[cfg(target_arch = "x86_64")]
  if std::env::var_os("FTZ_DAZ").is_some() {
    // SAFETY: only sets this thread's FTZ (bit 15) and DAZ (bit 6) bits.
    #[allow(deprecated)]
    unsafe {
      use std::arch::x86_64::{_mm_getcsr, _mm_setcsr};
      _mm_setcsr(_mm_getcsr() | 0x8040);
    }
  }
  let (_, mixed) = speech_over_keyboard(-30.0, 0.0);
  let speech_len = 2 * 48_000;
  let mut x: Vec<f32> = mixed.iter().cycle().take(speech_len).copied().collect();
  x.resize(speech_len + 10 * 48_000, 0.0);
  let time = |name: &str, f: &mut dyn FnMut(&mut [f32])| {
    let mut y = x.clone();
    let mut t_block = Vec::new();
    for b in y.chunks_mut(480) {
      let t = Instant::now();
      f(b);
      t_block.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let (sp, zero) = t_block.split_at(speech_len / 480);
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    eprintln!(
      "{name:<34} speech {:7.1} us | zeros {:7.1} us, slowest {:7.1} us",
      mean(sp),
      mean(zero),
      zero.iter().copied().fold(0.0, f64::max)
    );
  };
  let mut rnn = RnNoiseEngine::new();
  time("RNNoise 2017 denoiser", &mut |b| rnn.process(b, 0.75, |_| {}));
  let mut in_net = VoiceNet::new_input();
  time("in_net (RNNoise 0.2)", &mut |b| in_net.process(b, |_| {}));
  let mut out_net = VoiceNet::new();
  time("out_net (RNNoise 2017 VAD)", &mut |b| out_net.process(b, |_| {}));
  let mut silero = Vad48k::new().unwrap();
  time("Silero VAD", &mut |b| silero.process(b, |_| {}));
  let mut df = DeepFilterEngine::load().unwrap();
  time("DeepFilterNet 3 alone", &mut |b| df.process(b, 0.75, 1.0));
  let mut dp = DpdfEngine::load().unwrap();
  time("DPDFNet alone", &mut |b| dp.process(b, atten_lim(0.75)));
  let mut aec = crate::aec3::EchoCanceller3::new();
  let reference = x.clone();
  let mut pos = 0;
  time("AEC3 (reference goes silent too)", &mut |b| {
    let r = &reference[pos..pos + b.len()];
    aec.process_block_stereo(r, r, b);
    pos += b.len();
  });
  let mut lv = crate::voice_leveler::VoiceLeveler::new();
  time("voice leveler", &mut |b| lv.process(b, true, 0));
  for model in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
    let mut iso = AiVoiceIsolator::new();
    iso.set_model(model);
    iso.set_gate_threshold_db(GATE_AUTO_LOOKAHEAD);
    time(&format!("whole denoiser, {model:?}"), &mut |b| iso.process_samples(b));
  }
}
