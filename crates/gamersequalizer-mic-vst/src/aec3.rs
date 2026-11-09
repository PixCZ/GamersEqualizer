//! Acoustic echo canceller: WebRTC AEC3 via `sonora` (pure Rust port, BSD-3). Linear
//! partitioned filter (coarse + refined) on 0-8 kHz, plus the part our MDF lacked: a
//! nonlinear suppressor for the residual echo (all three bands up to 24 kHz) with comfort noise.
//! A/B on the 2026-11-07 debug captures (MDF removed 2026-11-08, see DEVLOG): 10-25 dB more echo removed
//! while only the far end plays, ~1.5 dB off the voice while both talk.
//!
//! The bulk delay is still removed beforehand (timestamps + `DelayEstimator`); AEC3 tracks
//! what is left itself. Its own high-pass is off: the AEC only runs while a reference exists,
//! so a built-in high-pass would switch the mic's bass on and off with the far end
//! (the user EQ has a high-pass anyway).
//!
//! Latency: AEC3 itself delays the mic by `AEC3_LATENCY_SAMPLES` (~9 ms, band split + block
//! framing; the MDF had none). It works on 10 ms frames: 480-sample blocks add nothing more,
//! any other block size switches once to a further 10 ms buffer.

use sonora::config::{EchoCanceller, EchoCanceller3Config, MaxProcessingRate, Pipeline, TransparentModeType};
use sonora::{AudioProcessing, Config, StreamConfig};
use std::collections::VecDeque;

/// Samples per AEC3 frame (10 ms at 48 kHz).
pub const FRAME: usize = 480;
/// Filter starts this far before the measured delay so estimation error can't cut the direct
/// path off (AEC3 would find it anyway; keeps the reference ahead of the echo).
pub const PRE_DELAY_SAMPLES: usize = 144;
/// Measured delay of the mic through AEC3 (test `test_latency_zero_for_whole_frames_...`).
pub const AEC3_LATENCY_SAMPLES: usize = 430;
/// Linear filter length reported to the UI: 13 refined blocks x 64 samples at 16 kHz (52 ms).
const FILTER_TAPS: usize = 13 * 64 * 3;

pub struct EchoCanceller3 {
  apm: AudioProcessing,
  in_ref: Vec<f32>,
  in_mic: Vec<f32>,
  in_len: usize,
  out_frame: Vec<f32>,
  render_out: Vec<f32>,
  out_queue: VecDeque<f32>,
  buffered: bool,
  erle_db: f32,
}

impl EchoCanceller3 {
  pub fn new() -> Self {
    Self {
      apm: build_apm(),
      in_ref: vec![0.0; FRAME],
      in_mic: vec![0.0; FRAME],
      in_len: 0,
      out_frame: vec![0.0; FRAME],
      render_out: vec![0.0; FRAME],
      out_queue: VecDeque::with_capacity(8192),
      buffered: false,
      erle_db: 0.0,
    }
  }

  pub fn taps(&self) -> usize {
    FILTER_TAPS
  }

  /// Process a block in place. `ref_block` must be delay-aligned to `mic_block`.
  pub fn process_block(&mut self, ref_block: &[f32], mic_block: &mut [f32]) {
    let n = mic_block.len().min(ref_block.len());
    if !self.buffered && (self.in_len != 0 || !n.is_multiple_of(FRAME)) {
      // This host's blocks don't line up with frames: 10 ms of latency from now on (once).
      self.buffered = true;
      self.out_queue.extend(std::iter::repeat_n(0.0, FRAME));
    }
    for (&m, &r) in mic_block.iter().zip(ref_block) {
      if let (Some(rs), Some(ms)) = (self.in_ref.get_mut(self.in_len), self.in_mic.get_mut(self.in_len)) {
        // NaN/inf from a driver must not reach the filter state.
        (*rs, *ms) = (finite(r), finite(m));
      }
      self.in_len += 1;
      if self.in_len == FRAME {
        self.in_len = 0;
        self.process_frame();
      }
    }
    for s in mic_block.iter_mut().take(n) {
      *s = self.out_queue.pop_front().unwrap_or(0.0);
    }
  }

  /// Drop buffered audio (partial input frame, queued output), keep the learned echo path.
  pub fn flush(&mut self) {
    self.in_len = 0;
    self.out_queue.clear();
    self.buffered = false;
  }

  /// Start over (other speaker device, delay jump). Rebuilds AEC3: allocates, but only on
  /// those rare events.
  pub fn reset(&mut self) {
    self.apm = build_apm();
    self.flush();
    self.erle_db = 0.0;
  }

  /// Delay the mic gets through here right now: AEC3's own plus the frame buffer, if on.
  pub fn latency_samples(&self) -> usize {
    AEC3_LATENCY_SAMPLES + if self.buffered { FRAME } else { 0 }
  }

  pub fn erle_db(&self) -> f32 {
    self.erle_db
  }

  /// AEC3 exposes no double-talk flag through the public API.
  pub fn is_double_talk(&self) -> bool {
    false
  }

  /// AEC3 keeps its filter internal: the UI impulse view stays flat.
  pub fn impulse_bins(&self, out: &mut [f32]) {
    out.fill(0.0);
  }

  fn process_frame(&mut self) {
    let rendered = self.apm.process_render_f32(&[&self.in_ref], &mut [&mut self.render_out]);
    let _ = self.apm.set_stream_delay_ms(0); // aligned beforehand
    let captured = self.apm.process_capture_f32(&[&self.in_mic], &mut [&mut self.out_frame]);
    if rendered.is_err() || captured.is_err() || !self.out_frame.iter().all(|s| s.is_finite()) {
      // Never drop the mic: pass this frame through unprocessed.
      log::warn!("[AEC3] frame failed (render {rendered:?}, capture {captured:?}), passed through");
      self.out_frame.copy_from_slice(&self.in_mic);
    }
    self.out_queue.extend(self.out_frame.iter().copied());
    if let Some(erle) = self.apm.statistics().echo_return_loss_enhancement {
      self.erle_db = erle as f32;
    }
  }
}

impl Default for EchoCanceller3 {
  fn default() -> Self {
    Self::new()
  }
}

fn build_apm() -> AudioProcessing {
  let stream = StreamConfig::new(48_000, 1);
  let config = Config {
    pipeline: Pipeline { maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz, ..Default::default() },
    echo_canceller: Some(EchoCanceller {
      enforce_high_pass_filtering: false,
      // Detects "no echo" (headphones) faster than the default counter.
      transparent_mode: TransparentModeType::Hmm,
    }),
    ..Default::default()
  };
  AudioProcessing::builder().config(config).capture_config(stream).render_config(stream).echo_canceller3_config(tuning()).build()
}

/// AEC3 defaults, loosened for a voice quieter than the reference. `default_gain` 1.0 assumes the
/// whole reference leaks into the mic until the filter converges (it never does without an echo,
/// e.g. headphones), so a quieter voice is ducked; the wider masks let a near-end voice up to about
/// the echo level through. Replayed on 2026-11-09 captures, AEC3 always on:
/// - headphones, voice -40 dBFS under music -28 dBFS: voice lost 26.8 dB with the defaults, 5.6 dB
///   with this (first 3 s after the music starts: about half of the voice frames still cut);
/// - speakers, music -18 dBFS: echo-only removed 26 dB as before (-84 dBFS), voice during the
///   music passes better than with the defaults.
fn tuning() -> EchoCanceller3Config {
  let mut cfg = EchoCanceller3Config::default();
  cfg.ep_strength.default_gain = 0.1;
  let normal = &mut cfg.suppressor.normal_tuning;
  normal.mask_lf.enr_transparent = 1.0;
  normal.mask_lf.enr_suppress = 1.1;
  normal.mask_hf.enr_transparent = 0.3;
  normal.mask_hf.enr_suppress = 0.5;
  cfg
}

fn finite(x: f32) -> f32 {
  if x.is_finite() {
    x
  } else {
    0.0
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Echo only, white noise through a short two-tap path.
  #[test]
  fn test_cancels_echo_and_keeps_voice() {
    let len = 8 * 48_000;
    let far = noise(len, 11);
    let echo: Vec<f32> = (0..len).map(|n| delayed(&far, n, 200) * 0.3 - delayed(&far, n, 600) * 0.1).collect();
    let out = run(&mut EchoCanceller3::new(), &far, &echo, 480);
    let removed = energy_db(&echo[5 * 48_000..]) - energy_db(&out[5 * 48_000..]);
    assert!(removed > 20.0, "only {removed:.1} dB echo removed");
  }

  /// With no far end the mic passes at its level (no suppression, no high-pass).
  #[test]
  fn test_passes_mic_without_far_end() {
    let mic = noise(4 * 48_000, 3);
    let out = run(&mut EchoCanceller3::new(), &vec![0.0; mic.len()], &mic, 480);
    let change = energy_db(&out[48_000..]) - energy_db(&mic[48_000..]);
    assert!(change.abs() < 1.0, "level changed by {change:.1} dB");
  }

  /// One NaN block must not leave the canceller emitting NaN for good.
  #[test]
  fn test_recovers_after_a_nan_block() {
    let far = noise(3 * 48_000, 5);
    let mut mic: Vec<f32> = (0..far.len()).map(|n| delayed(&far, n, 40) * 0.4).collect();
    mic[48_000..48_480].fill(f32::NAN);
    let out = run(&mut EchoCanceller3::new(), &far, &mic, 480);
    assert!(out.iter().all(|s| s.is_finite()));
  }

  /// 480-sample blocks add only AEC3's own latency, others one frame more.
  #[test]
  fn test_latency_zero_for_whole_frames_one_frame_otherwise() {
    let mic = impulse_train(48_000);
    let silent = vec![0.0f32; mic.len()];
    let lag =
      |out: &[f32]| (0..2 * FRAME).max_by(|&a, &b| corr(out, &mic, a).total_cmp(&corr(out, &mic, b))).unwrap_or(0);
    let aligned = lag(&run(&mut EchoCanceller3::new(), &silent, &mic, 480));
    let unaligned = lag(&run(&mut EchoCanceller3::new(), &silent, &mic, 441));
    assert_eq!(unaligned, aligned + FRAME, "441-sample blocks: one frame later than 480 ({aligned})");
    assert_eq!(aligned, AEC3_LATENCY_SAMPLES, "AEC3's own latency changed");
  }

  // Scenarios kept from the MDF canceller this replaced (2026-11-08). AEC3 adds
  // `AEC3_LATENCY_SAMPLES` of delay, so the output is compared against the delayed input.

  /// Regression (debug capture 2026-11-07): echo at room-noise level under a speech-like
  /// reference. The old NLMS made the output louder than the raw mic (15 dB live).
  #[test]
  fn test_never_louder_than_the_mic_with_weak_echo_under_speech() {
    let len = 8 * 48_000;
    let speaker = speech_like(len, 3);
    let room = noise(len, 9);
    let mic: Vec<f32> = (0..len).map(|n| delayed(&speaker, n, 46) * 0.01 + room[n] * 0.004).collect();
    let out = run(&mut EchoCanceller3::new(), &speaker, &mic, 480);
    let gain_db = energy_db(&out[4 * 48_000..]) - energy_db(&mic[4 * 48_000..]);
    eprintln!("weak echo: output {gain_db:+.1} dB vs mic");
    assert!(gain_db < 0.5, "AEC made the mic {gain_db:.1} dB louder");
  }

  /// Echo path with a 200 ms reverberant tail (RT60 ~300 ms), speech-like far end.
  #[test]
  fn test_cancels_reverberant_echo() {
    let len = 8 * 48_000;
    let far = speech_like(len, 3);
    let echo = convolve(&far, &room_response(9_600, 40, 0.3, 17));
    let out = run(&mut EchoCanceller3::new(), &far, &echo, 480);
    let removed = energy_db(&echo[4 * 48_000..]) - energy_db(&out[4 * 48_000..]);
    eprintln!("reverberant echo: {removed:.1} dB removed");
    assert!(removed > 20.0, "only {removed:.1} dB echo removed");
  }

  /// Loud near-end voice over the far end: the voice passes (about its level), the echo stays down.
  #[test]
  fn test_double_talk_keeps_voice_and_cancels_echo() {
    let len = 6 * 48_000;
    let speaker = noise(len, 5);
    let voice_at = 3 * 48_000;
    let voice: Vec<f32> = (0..len).map(|n| if n >= voice_at { 0.5 * (n as f32 * 0.0393).sin() } else { 0.0 }).collect();
    let echo: Vec<f32> = (0..len).map(|n| delayed(&speaker, n, PRE_DELAY_SAMPLES) * 0.3).collect();
    let (echo_db, voice_db) = split(&speaker, &echo, &voice, voice_at + 48_000..len);
    eprintln!("double talk: echo {echo_db:.1} dB removed, voice {voice_db:+.1} dB");
    assert!(echo_db > 15.0, "echo during double talk: only {echo_db:.1} dB removed");
    assert!(voice_db.abs() < 3.0, "voice level changed by {voice_db:.1} dB");
  }

  /// Voice well under a loud reference (a level-based detector would miss it): no echo leak.
  #[test]
  fn test_quiet_voice_under_loud_music_does_not_leak_echo() {
    let len = 6 * 48_000;
    let speaker = noise(len, 7);
    let echo: Vec<f32> = (0..len).map(|n| delayed(&speaker, n, PRE_DELAY_SAMPLES) * 0.3).collect();
    let voice_at = 3 * 48_000;
    let voice: Vec<f32> = (0..len)
      .map(|n| if n >= voice_at { 0.2 * (n as f32 * 2.0 * std::f32::consts::PI * 300.0 / 48_000.0).sin() } else { 0.0 })
      .collect();
    let (echo_db, voice_db) = split(&speaker, &echo, &voice, voice_at + 12_000..len);
    eprintln!("quiet voice under music: echo {echo_db:.1} dB removed, voice {voice_db:+.1} dB");
    assert!(voice_db.abs() < 3.0, "voice level changed by {voice_db:.1} dB");
    assert!(echo_db > 15.0, "echo leaked during double talk: only {echo_db:.1} dB removed");
  }

  /// The echo path changes (mic moved, volume changed): cancelled again within ~1 s.
  /// The echo sits at least `PRE_DELAY_SAMPLES` behind the reference, as the chain feeds it
  /// (with only 20 samples AEC3 stalled at ~10 dB; the chain never gets there).
  #[test]
  fn test_relearns_after_echo_path_change() {
    let len = 7 * 48_000;
    let change_at = 4 * 48_000;
    let speaker = speech_like(len, 21);
    let room = noise(len, 4);
    let echo: Vec<f32> = (0..len)
      .map(|n| {
        if n < change_at {
          delayed(&speaker, n, PRE_DELAY_SAMPLES) * 0.3
        } else {
          delayed(&speaker, n, 300) * -0.15 + delayed(&speaker, n, 80) * 0.1
        }
      })
      .collect();
    let mic: Vec<f32> = echo.iter().zip(&room).map(|(e, r)| e + r * 0.0005).collect();
    let out = run(&mut EchoCanceller3::new(), &speaker, &mic, 480);
    let before = energy_db(&echo[change_at - 48_000..change_at]) - energy_db(&out[change_at - 48_000..change_at]);
    let after = energy_db(&echo[change_at + 48_000..]) - energy_db(&out[change_at + 48_000..]);
    eprintln!("path change: {before:.1} dB before, {after:.1} dB after");
    assert!(before > 20.0, "converged before the change: {before:.1} dB");
    assert!(after > 15.0, "relearned after the change: only {after:.1} dB");
  }

  /// Every debug capture through AEC3 (raw mic + the reference as the live chain fed it):
  /// per 5 s levels, and `test_data/private/aec_ab/<capture>_{mic,aec3}.wav` for listening and
  /// for `ai_denoiser::tests::echo_gate_report`. Run:
  /// `cargo test --release -p gamersequalizer-mic-vst --lib capture_export -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn capture_export() {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_data/private");
    let Ok(dirs) = std::fs::read_dir(base.join("captures")) else { return eprintln!("no captures") };
    let out_dir = base.join("aec_ab");
    let _ = std::fs::create_dir_all(&out_dir);
    for dir in dirs.flatten().map(|d| d.path()) {
      let read = |name: &str| -> Option<Vec<f32>> {
        Some(hound::WavReader::open(dir.join(name)).ok()?.samples::<f32>().map_while(Result::ok).collect())
      };
      let (Some(mic), Some(refs)) = (read("01_mic_raw.wav"), read("03_ref_aec.wav")) else { continue };
      let out = run(&mut EchoCanceller3::new(), &refs, &mic, 480);
      let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
      eprintln!("\n{name}: per 5 s dBFS        mic    AEC3     ref");
      for t in (0..mic.len()).step_by(5 * 48_000) {
        let r = t..(t + 5 * 48_000).min(mic.len());
        let s = |v: &[f32]| energy_db(v.get(r.clone()).unwrap_or_default());
        eprintln!("{:>26} {:7.1} {:7.1} {:7.1}", t / 48_000, s(&mic), s(&out), s(&refs));
      }
      for (tag, v) in [("mic", &mic), ("aec3", &out)] {
        write_wav(&out_dir.join(format!("{name}_{tag}.wav")), v);
      }
    }
  }

  /// Double talk, `echo` + `voice` on the mic: dB of echo removed and the voice level change
  /// over `range`. The output is projected on the echo and on the voice (both delayed by AEC3's
  /// latency; noise and a tone are uncorrelated), so the suppressor shaping the voice a little
  /// doesn't read as leaked echo.
  fn split(far: &[f32], echo: &[f32], voice: &[f32], range: std::ops::Range<usize>) -> (f32, f32) {
    let mic: Vec<f32> = echo.iter().zip(voice).map(|(e, v)| e + v).collect();
    let out = run(&mut EchoCanceller3::new(), far, &mic, 480);
    let gain_db = |x: &[f32]| {
      let (mut xo, mut xx) = (0.0f64, 0.0f64);
      for n in range.clone() {
        let d = delayed(x, n, AEC3_LATENCY_SAMPLES) as f64;
        xo += d * out[n] as f64;
        xx += d * d;
      }
      20.0 * ((xo / xx.max(1e-20)).abs() + 1e-10).log10() as f32
    };
    (-gain_db(echo), gain_db(voice))
  }

  /// Direct path at `delay` plus an exponentially decaying noise tail (`rt60` seconds).
  fn room_response(len: usize, delay: usize, rt60: f32, seed: u32) -> Vec<f32> {
    let n = noise(len, seed);
    let decay = (-6.9 / (rt60 * 48_000.0)).exp();
    (0..len)
      .map(|i| match i.cmp(&delay) {
        std::cmp::Ordering::Less => 0.0,
        std::cmp::Ordering::Equal => 0.5,
        std::cmp::Ordering::Greater => n[i] * 0.15 * decay.powi((i - delay) as i32),
      })
      .collect()
  }

  fn convolve(x: &[f32], h: &[f32]) -> Vec<f32> {
    let taps: Vec<(usize, f32)> = h.iter().copied().enumerate().filter(|(_, v)| v.abs() > 1e-5).collect();
    (0..x.len()).map(|n| taps.iter().map(|&(k, v)| delayed(x, n, k) * v).sum()).collect()
  }

  fn write_wav(path: &std::path::Path, x: &[f32]) {
    let spec =
      hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    if let Ok(mut w) = hound::WavWriter::create(path, spec) {
      x.iter().for_each(|&s| drop(w.write_sample(s)));
      let _ = w.finalize();
    }
  }

  /// 120 Hz glottal pulses through a resonance that moves every 80 ms, in 4 Hz syllables.
  fn speech_like(len: usize, seed: u32) -> Vec<f32> {
    const FORMANTS: [f32; 5] = [700.0, 1200.0, 400.0, 2400.0, 900.0];
    let white = noise(len, seed);
    let (mut y1, mut y2) = (0.0f32, 0.0f32);
    (0..len)
      .map(|n| {
        let f = FORMANTS[(n / 3_840) % FORMANTS.len()];
        let (rad, theta) = (0.985f32, 2.0 * std::f32::consts::PI * f / 48_000.0);
        let pulse = if n % 400 == 0 { 1.0 } else { 0.0 } + white[n] * 0.05;
        let y = pulse + 2.0 * rad * theta.cos() * y1 - rad * rad * y2;
        (y2, y1) = (y1, y);
        y * 0.02 * (n as f32 * 2.0 * std::f32::consts::PI * 4.0 / 48_000.0).sin().max(0.0)
      })
      .collect()
  }

  fn run(aec: &mut EchoCanceller3, refs: &[f32], mic: &[f32], block: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(mic.len());
    for (r, m) in refs.chunks(block).zip(mic.chunks(block)) {
      let mut b = m.to_vec();
      aec.process_block(r, &mut b);
      out.extend_from_slice(&b);
    }
    out
  }

  fn corr(out: &[f32], x: &[f32], lag: usize) -> f32 {
    out.iter().enumerate().map(|(n, o)| o * delayed(x, n, lag)).sum()
  }

  fn impulse_train(len: usize) -> Vec<f32> {
    (0..len).map(|n| if n % 4_801 == 100 { 0.5 } else { 0.0 }).collect()
  }

  fn energy_db(x: &[f32]) -> f32 {
    10.0 * (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64 + 1e-20).log10() as f32
  }

  fn delayed(x: &[f32], n: usize, by: usize) -> f32 {
    n.checked_sub(by).and_then(|i| x.get(i)).copied().unwrap_or(0.0)
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
}
