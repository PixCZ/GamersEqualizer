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
//!
//! Mic gain: AEC3 is tuned for call levels and has absolute thresholds (it accepts a converged
//! filter only on mic blocks over -56 dBFS). A quiet mic keeps it off its linear filter, and it
//! then ducks the voice by the reference's level. So the mic is raised on its way in and lowered
//! again on its way out (`set_mic_gain_db`), like DeepFilterNet's pre-gain.

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
/// Highest mic gain into AEC3 (the leveler's highest boost).
pub const MAX_MIC_GAIN_DB: f32 = 24.0;
/// The mic gain moves in these steps, and only when the request is this far past half a step:
/// every change rebuilds AEC3 (its fallback echo gain is scaled with it), which costs its
/// learned echo path and `AEC3_LATENCY_SAMPLES` of silence.
const MIC_GAIN_STEP_DB: f32 = 6.0;
const MIC_GAIN_HYSTERESIS_DB: f32 = 1.5;
/// Frames (10 ms) another step has to be wanted before the rebuild...
const MIC_GAIN_LATCH_FRAMES: u32 = 200;
/// ...which then waits for two frames this far under the loudest one since (a pause between
/// words, so the silence is not heard), at most twice as long again.
const MIC_GAIN_PAUSE_RATIO: f32 = 0.125;
/// Frames after a start, a `reset` or a gap (`flush`) in which a new step is taken at once:
/// little is learned yet, and the leveler's remembered level arrives a few blocks into a stream.
const MIC_GAIN_FREE_FRAMES: u32 = 50;
/// Peak the raised mic may reach inside AEC3: it clamps at 1.0, and takes 32700 / 32768 for
/// a saturated capture.
const MIC_PEAK_LIMIT: f32 = 0.9;
/// `ep_strength.default_gain` at mic gain 0 dB, see `tuning`.
const FALLBACK_ECHO_GAIN: f32 = 0.01;

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
  /// Mic gain AEC3 was built for (dB, a multiple of `MIC_GAIN_STEP_DB`) and as a factor.
  mic_gain_db: f32,
  mic_gain: f32,
  /// Step asked for, frames it has differed from `mic_gain_db`, loudest mic frame since.
  wanted_gain_db: f32,
  wanted_frames: u32,
  wanted_peak: f32,
  /// Frames left of `MIC_GAIN_FREE_FRAMES`.
  free_frames: u32,
  /// Peak of the previous mic frame.
  last_peak: f32,
  /// Gain of the previous frame (a loud frame gets less, see `process_frame`).
  last_frame_gain: f32,
  /// Test-only overrides of `tuning` (`tests::apply_variant`).
  #[cfg(test)]
  variant: String,
}

impl EchoCanceller3 {
  pub fn new() -> Self {
    Self {
      apm: build_apm(tuning(1.0)),
      in_ref: vec![0.0; FRAME],
      in_mic: vec![0.0; FRAME],
      in_len: 0,
      out_frame: vec![0.0; FRAME],
      render_out: vec![0.0; FRAME],
      out_queue: VecDeque::with_capacity(8192),
      buffered: false,
      erle_db: 0.0,
      mic_gain_db: 0.0,
      mic_gain: 1.0,
      wanted_gain_db: 0.0,
      wanted_frames: 0,
      wanted_peak: 0.0,
      free_frames: MIC_GAIN_FREE_FRAMES,
      last_peak: 0.0,
      last_frame_gain: 1.0,
      #[cfg(test)]
      variant: String::new(),
    }
  }

  pub fn taps(&self) -> usize {
    FILTER_TAPS
  }

  /// Gain that brings the user's speech to a normal level (the leveler's boost, dB): the mic
  /// goes into AEC3 this much louder and comes out this much quieter again. Cheap to call every
  /// block; AEC3 follows in 6 dB steps, at once in its first 0.5 s and after 2 s otherwise.
  pub fn set_mic_gain_db(&mut self, gain_db: f32) {
    let want = if gain_db.is_finite() { gain_db.clamp(0.0, MAX_MIC_GAIN_DB) } else { 0.0 };
    if (want - self.wanted_gain_db).abs() >= MIC_GAIN_STEP_DB / 2.0 + MIC_GAIN_HYSTERESIS_DB {
      self.wanted_gain_db = (want / MIC_GAIN_STEP_DB).round() * MIC_GAIN_STEP_DB;
    }
  }

  /// Mic gain AEC3 runs with right now (dB).
  pub fn mic_gain_db(&self) -> f32 {
    self.mic_gain_db
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
    self.free_frames = MIC_GAIN_FREE_FRAMES;
  }

  /// Start over (other speaker device, delay jump). Rebuilds AEC3: allocates, but only on
  /// those rare events.
  pub fn reset(&mut self) {
    self.rebuild();
    self.flush();
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
    let peak = self.in_mic.iter().fold(0.0f32, |p, s| p.max(s.abs()));
    self.follow_wanted_gain(peak);
    // A shout must not clip inside AEC3: that frame gets less gain (never under 1, so without
    // a mic gain nothing changes).
    let gain = if peak * self.mic_gain > MIC_PEAK_LIMIT { (MIC_PEAK_LIMIT / peak).max(1.0) } else { self.mic_gain };
    self.in_mic.iter_mut().for_each(|s| *s *= gain);

    let rendered = self.apm.process_render_f32(&[&self.in_ref], &mut [&mut self.render_out]);
    let _ = self.apm.set_stream_delay_ms(0); // aligned beforehand
    let captured = self.apm.process_capture_f32(&[&self.in_mic], &mut [&mut self.out_frame]);
    let failed = rendered.is_err() || captured.is_err() || !self.out_frame.iter().all(|s| s.is_finite());
    if failed {
      // Never drop the mic: pass this frame through unprocessed.
      log::warn!("[AEC3] frame failed (render {rendered:?}, capture {captured:?}), passed through");
      self.out_frame.copy_from_slice(&self.in_mic);
    }
    // AEC3 puts the mic out `AEC3_LATENCY_SAMPLES` later: the start of its frame is still the
    // previous input frame, with that frame's gain.
    let from_last = if failed { 0 } else { AEC3_LATENCY_SAMPLES };
    for (i, s) in self.out_frame.iter_mut().enumerate() {
      *s /= if i < from_last { self.last_frame_gain } else { gain };
    }
    self.last_frame_gain = gain;
    self.free_frames = self.free_frames.saturating_sub(1);
    self.out_queue.extend(self.out_frame.iter().copied());
    if let Some(erle) = self.apm.statistics().echo_return_loss_enhancement {
      self.erle_db = erle as f32;
    }
  }

  /// Rebuild AEC3 for the wanted mic gain once it is due. `peak`: this frame's mic peak.
  fn follow_wanted_gain(&mut self, peak: f32) {
    let last_peak = std::mem::replace(&mut self.last_peak, peak);
    if self.wanted_gain_db == self.mic_gain_db {
      (self.wanted_frames, self.wanted_peak) = (0, 0.0);
      return;
    }
    self.wanted_frames += 1;
    self.wanted_peak = self.wanted_peak.max(peak);
    // The old AEC3 still holds the end of the last frame, which is lost: that one counts too.
    let pause =
      peak.max(last_peak) <= self.wanted_peak * MIC_GAIN_PAUSE_RATIO || self.wanted_frames >= 3 * MIC_GAIN_LATCH_FRAMES;
    if self.free_frames > 0 || (self.wanted_frames >= MIC_GAIN_LATCH_FRAMES && pause) {
      log::info!("[AEC3] mic gain {} -> {} dB", self.mic_gain_db, self.wanted_gain_db);
      self.rebuild();
    }
  }

  /// New AEC3 for the wanted mic gain. Allocates on the audio thread like `reset`: at a stream's
  /// start, and when the learned speech level moves by a step (once per 2 s at most).
  fn rebuild(&mut self) {
    self.mic_gain_db = self.wanted_gain_db;
    self.mic_gain = 10f32.powf(self.mic_gain_db / 20.0);
    #[allow(unused_mut)] // only the tests change it
    let mut config = tuning(self.mic_gain);
    #[cfg(test)]
    tests::apply_variant(&mut config, &self.variant, self.mic_gain);
    self.apm = build_apm(config);
    self.erle_db = 0.0;
    (self.wanted_frames, self.wanted_peak) = (0, 0.0);
  }
}

impl Default for EchoCanceller3 {
  fn default() -> Self {
    Self::new()
  }
}

fn build_apm(tuning: EchoCanceller3Config) -> AudioProcessing {
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
  AudioProcessing::builder()
    .config(config)
    .capture_config(stream)
    .render_config(stream)
    .echo_canceller3_config(tuning)
    .build()
}

/// AEC3 defaults, loosened for a voice quieter than the reference. While AEC3 cannot use its
/// linear filter it takes `default_gain` x the reference for the echo. That is the case without
/// an echo (headphones) and with a weak one: capture 2026-11-09 21:06, music from the speakers
/// 50 dB down at the mic. The default 1.0 (the whole reference leaks in) ducks any quieter voice.
/// 0.1 was still 30 dB over that real echo: the first 200 ms of a word at -46 dBFS after a pause
/// came out at -81 dBFS, and clicks at -40 dBFS at -54..-77. With 0.01 (`capture_replay_report`)
/// the word's onset passes (-54.9 / -52.1 dBFS in its two 100 ms windows) and echo alone is
/// removed as before on all 15 captures with a reference (first 5 s, strongest echo: -72.9
/// dBFS both; 0.003 lets it up to -68.6). The unit tests measure the same except the weak echo
/// under speech (-6.4 -> -2.5 dB vs the mic: less of the voice taken). The wider masks let a
/// near-end voice up to about the echo level through. Replayed on 2026-11-09 captures, AEC3
/// always on, at 0.1:
/// - headphones, voice -40 dBFS under music -28 dBFS: voice lost 26.8 dB with the defaults, 5.6 dB
///   with this (first 3 s after the music starts: about half of the voice frames still cut);
/// - speakers, music -18 dBFS: echo-only removed 26 dB as before (-84 dBFS), voice during the
///   music passes better than with the defaults.
///
/// `mic_gain`: factor the mic is raised by on its way in (`set_mic_gain_db`). The fallback gain
/// is relative to the mic AEC3 sees, so it goes up with it and the estimate stays where the
/// captures above had it. AEC3's own "no echo" mode has a fixed 0.01 and so gets looser.
///
/// Tuning from 2026-11-07 and 2026-11-09 captures (`capture_tuning_report`, milestone 44):
/// - `erle.max_l = 30.0`, `max_h = 10.0`: AEC3 trusts its linear filter up to 30 dB (below 4 kHz)
///   and 10 dB (above).
/// - `bands_in_limiting_gain = 0`: bins over 2 kHz no longer follow the 2 kHz bin's gain,
///   recovering 5-8 dB in the 4-8 kHz band and reducing voice loss across speakers and HDMI monitor
///   captures:
///   - `122712` (speakers): 4-8 kHz loss 16.2 -> 8.1 dB, voice loss 8.3 -> 6.6 dB, cuts > 10 dB 28 -> 18 %;
///   - `125717` (speakers): 4-8 kHz loss 11.0 -> 4.9 dB, voice loss 5.9 -> 4.7 dB, cuts > 10 dB 24 -> 17 %;
///   - `130319` (speakers): 4-8 kHz loss 7.0 -> 6.0 dB, voice loss 3.4 -> 3.0 dB;
///   - `205013` (speakers, quiet mic): 4-8 kHz loss 9.0 -> 6.7 dB, voice loss 1.9 -> 1.7 dB;
///   - `210655` (speakers, quiet mic): 4-8 kHz loss 6.7 -> 5.1 dB, voice loss 2.2 -> 2.0 dB;
///   - `101152` (HDMI monitor): 4-8 kHz loss 16.9 -> 10.9 dB, voice loss 7.7 -> 6.0 dB, cuts > 10 dB 29 -> 21 %;
///   - `102403` (HDMI monitor): 4-8 kHz loss 11.5 -> 6.5 dB, voice loss 5.9 -> 4.7 dB;
///   - `115931` & `122520` (headphones): voice loss 0.6 and 0.7 dB unchanged.
///
/// In the full chain with denoiser (`ai_denoiser::tests::capture_replay_report`), mean voice loss
/// drops on `122712` from 7.7 to 5.1 dB and on `125717` from 8.6 to 6.5 dB with zero leak increase
/// and pauses remaining at -107 to -127 dBFS.
fn tuning(mic_gain: f32) -> EchoCanceller3Config {
  let mut cfg = EchoCanceller3Config::default();
  cfg.ep_strength.default_gain = FALLBACK_ECHO_GAIN * mic_gain;
  cfg.erle.max_l = 30.0;
  cfg.erle.max_h = 10.0;
  cfg.suppressor.high_frequency_suppression.bands_in_limiting_gain = 0;
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
  use gamersequalizer_common::biquad::BiquadCascade;
  use gamersequalizer_common::profile::{BiquadBand, FilterType};
  use gamersequalizer_common::shm::MIC_LEVELER_DEFAULT_TARGET_DB_X10;
  use std::path::Path;

  /// 100 ms: the window `capture_tuning_report` sorts and measures in.
  const WINDOW: usize = 4_800;
  /// Windows it leaves out at the start: the replay starts cold, AEC3's initial phase is 2.5 s.
  const COLD_WINDOWS: usize = 30;

  /// What Silero hears on the raw mic in a window of a capture.
  #[derive(Clone, Copy, PartialEq)]
  enum Heard {
    Voice,
    /// No voice while the reference plays.
    EchoAlone,
    Other,
  }

  /// `quiet_mic`'s result: dB of echo removed while only the far end plays, and the voice's
  /// level change (all of it, its 4-8 kHz band) while both do.
  #[derive(Debug)]
  struct QuietMic {
    echo_removed_db: f32,
    voice_db: f32,
    voice_hf_db: f32,
  }

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

  // Mic gain (2026-11-10). The 2026-11-09 captures have a quiet mic: voice -45..-60 dBFS, the
  // music's echo -55..-70 dBFS.

  /// Regression: with the echo at -68 dBFS and the voice at -46 dBFS (capture 2026-11-09 21:06)
  /// AEC3 took 3.8 dB off the voice and 6.4 dB off its 4-8 kHz band (down from 5.4 / 19 dB
  /// before the 2026-11-10 tuning decoupled HF suppression). With the leveler's boost as
  /// mic gain (24 dB): 1.2 and 1.6 dB loss, the echo removed as before (14 dB, down to the room noise).
  #[test]
  fn test_quiet_mic_keeps_the_voice_with_mic_gain() {
    let (plain, gained) = (quiet_mic(-68.0, -46.0, 0.0), quiet_mic(-68.0, -46.0, 24.0));
    eprintln!("quiet mic, no gain: {plain:?}\nquiet mic, 24 dB:   {gained:?}");
    assert!(
      plain.voice_hf_db < gained.voice_hf_db - 3.0,
      "the mic gain should keep at least 3 dB more HF voice: {plain:?} vs {gained:?}"
    );
    assert!(gained.voice_db > -2.5 && gained.voice_hf_db > -4.0, "voice lost with the mic gain: {gained:?}");
    assert!(gained.echo_removed_db > plain.echo_removed_db - 1.0, "echo leaks with the mic gain: {gained:?}");
  }

  /// At call levels (echo -40 dBFS, voice -30 dBFS) the leveler asks for little: no harm.
  #[test]
  fn test_mic_gain_changes_little_at_call_levels() {
    let (plain, gained) = (quiet_mic(-40.0, -30.0, 0.0), quiet_mic(-40.0, -30.0, 6.0));
    eprintln!("call levels, no gain: {plain:?}\ncall levels, 6 dB:    {gained:?}");
    assert!((gained.voice_db - plain.voice_db).abs() < 1.0, "voice: {plain:?} -> {gained:?}");
    assert!(gained.echo_removed_db > plain.echo_removed_db - 1.0, "echo: {plain:?} -> {gained:?}");
  }

  /// A shout 46 dB over the level the gain was set for would clip inside AEC3 (it clamps at
  /// full scale): that frame gets less gain instead, and comes out as it went in.
  #[test]
  fn test_mic_gain_does_not_clip_a_shout() {
    let len = 2 * 48_000;
    let mic: Vec<f32> = (0..len).map(|n| if n >= 48_000 { 0.8 } else { 0.004 } * (n as f32 * 0.0393).sin()).collect();
    let error = |gain_db: f32, block: usize| {
      let mut aec = EchoCanceller3::new();
      aec.set_mic_gain_db(gain_db);
      let out = run(&mut aec, &vec![0.0; len], &mic, block);
      assert_eq!(aec.mic_gain_db(), gain_db);
      (48_000..out.len()).map(|n| (out[n] - delayed(&mic, n, aec.latency_samples())).abs()).fold(0.0f32, f32::max)
    };
    // 441: blocks that don't line up with AEC3's frames.
    for block in [480, 441] {
      let (plain, gained) = (error(0.0, block), error(24.0, block));
      eprintln!("shout, {block} samples: worst sample error {plain:.4} without mic gain, {gained:.4} with 24 dB");
      assert!(gained < plain + 0.02, "the shout is distorted: error {gained} (without mic gain {plain})");
    }
  }

  /// The gain follows the request in 6 dB steps: at once in the first 0.5 s (the leveler's
  /// remembered level arrives a few blocks late), then only after the request held for 2 s
  /// (every change rebuilds AEC3), and not for a small move.
  #[test]
  fn test_mic_gain_moves_in_steps_and_waits() {
    let mut aec = EchoCanceller3::new();
    let mut frame = |want: f32| {
      aec.set_mic_gain_db(want);
      aec.process_block(&[0.0; FRAME], &mut [0.0; FRAME]);
      aec.mic_gain_db()
    };
    assert_eq!(frame(0.0), 0.0);
    assert_eq!(frame(20.0), 18.0, "a few frames into the stream: at once");
    assert_eq!(frame(22.0), 18.0, "within the step's hysteresis");
    assert_eq!((0..MIC_GAIN_FREE_FRAMES).map(|_| frame(20.0)).last(), Some(18.0));
    assert_eq!(frame(f32::NAN), 18.0, "a new step waits");
    assert_eq!((1..MIC_GAIN_LATCH_FRAMES).map(|_| frame(0.0)).last(), Some(0.0), "after 2 s");
    assert_eq!(frame(99.0), 0.0);
    assert_eq!((1..MIC_GAIN_LATCH_FRAMES).map(|_| frame(99.0)).last(), Some(MAX_MIC_GAIN_DB), "capped");
  }

  /// A new step costs 9 ms of silence, so it waits for a pause: two quiet frames in a row (the
  /// old AEC3 still holds the end of the frame before), or 6 s of speech without one. After a
  /// gap in the reference (`flush`) it is taken at once.
  #[test]
  fn test_mic_gain_step_waits_for_a_pause() {
    let tone: Vec<f32> = (0..FRAME).map(|n| 0.1 * (n as f32 * 0.0393).sin()).collect();
    let frame = |aec: &mut EchoCanceller3, want: f32, mic: &[f32]| {
      aec.set_mic_gain_db(want);
      aec.process_block(&[0.0; FRAME], &mut mic.to_vec());
      aec.mic_gain_db()
    };
    let past_the_start = || {
      let mut aec = EchoCanceller3::new();
      for _ in 0..MIC_GAIN_FREE_FRAMES {
        frame(&mut aec, 0.0, &tone);
      }
      aec
    };
    let mut aec = past_the_start();
    assert!((0..MIC_GAIN_LATCH_FRAMES + 50).all(|_| frame(&mut aec, 12.0, &tone) == 0.0), "changed during speech");
    assert_eq!(frame(&mut aec, 12.0, &[0.0; FRAME]), 0.0, "the frame before was still loud");
    assert_eq!(frame(&mut aec, 12.0, &[0.0; FRAME]), 12.0, "second quiet frame");

    let mut aec = past_the_start();
    assert!((1..3 * MIC_GAIN_LATCH_FRAMES).all(|_| frame(&mut aec, 12.0, &tone) == 0.0), "changed during speech");
    assert_eq!(frame(&mut aec, 12.0, &tone), 12.0, "no pause for 6 s");

    let mut aec = past_the_start();
    assert_eq!(frame(&mut aec, 12.0, &tone), 0.0);
    aec.flush();
    assert_eq!(frame(&mut aec, 12.0, &tone), 12.0, "after a gap");
  }

  /// Every debug capture through AEC3 (raw mic + the reference as the live chain fed it, the
  /// capture's leveler gain as mic gain):
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
      let (Some(mic), Some(refs)) = (read_capture(&dir, "01_mic_raw.wav"), read_capture(&dir, "03_ref_aec.wav")) else {
        continue;
      };
      let out = run_gained(&mut EchoCanceller3::new(), &refs, &mic, &capture_gains(&dir, mic.len())).0;
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

  /// One debug capture through AEC3 as it is now (with the mic gain the chain would ask for:
  /// the capture's leveler gain), against what the chain's AEC put out live
  /// (`04_after_aec.wav`): the 100 ms windows more than 6 dB apart, then the level of the first
  /// 5 s (echo alone in most captures). For trying a tuning on a real capture.
  /// `CAP=20261109-210655 cargo test --release -p gamersequalizer-mic-vst --lib aec3::tests::capture_replay_report -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn capture_replay_report() {
    let cap = std::env::var("CAP").unwrap_or("20261109-210655".into());
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_data/private/captures").join(&cap);
    let read = |name: &str| read_capture(&dir, name);
    let (Some(mic), Some(refs), Some(live)) =
      (read("01_mic_raw.wav"), read("03_ref_aec.wav"), read("04_after_aec.wav"))
    else {
      return eprintln!("skipped: no capture {cap} in test_data/private/captures/");
    };
    let out = run_gained(&mut EchoCanceller3::new(), &refs, &mic, &capture_gains(&dir, mic.len())).0;
    let level = |x: &[f32], window: usize| energy_db(x.get(window * 4_800..(window + 1) * 4_800).unwrap_or_default());
    // The first second is AEC3 converging in the replay (the live one had its history).
    for window in 10..mic.len() / 4_800 {
      let (m, o, l) = (level(&mic, window), level(&out, window), level(&live, window));
      if (o - l).abs() > 6.0 && o.max(l) > -75.0 {
        eprintln!("{:5.1} s: mic {m:6.1}, live {l:6.1}, now {o:6.1} dBFS", window as f32 / 10.0);
      }
    }
    let first = 48_000..(5 * 48_000).min(mic.len());
    eprintln!(
      "1-5 s: mic {:.1}, live {:.1}, now {:.1} dBFS",
      energy_db(&mic[first.clone()]),
      energy_db(&live[first.clone()]),
      energy_db(&out[first])
    );
  }

  /// Tuning on the debug captures: each one (`CAP=a,b`: only those) through AEC3 once per
  /// variant (`VARIANTS`, `;` between them), one line each. A variant is a list of
  /// `apply_variant` keys; an empty one is AEC3 as built. The mic gain is the capture's leveler
  /// gain, as in the chain, or `pregain=<dB>` (0: none, as before 2026-11-10).
  ///
  /// 100 ms windows from 3 s on (the replay starts cold; AEC3's initial phase is 2.5 s),
  /// sorted by Silero on the raw mic (`sort_windows`). Columns:
  /// mic gain at the end; while the echo is alone: dB removed (median window) and the output
  /// level only every tenth window is over (dBFS: what is left for the denoiser); voice
  /// windows: mean level lost mic -> output (dB), share that lost over 6 and over 10 dB, loss
  /// in the first window of a word, loss of the 4-8 kHz band; AEC3's own ERLE estimate while
  /// the echo is alone (near 0: it is not using its linear filter). The voice windows hold the echo too,
  /// so their loss never reaches 0: compare variants, and `open` shows what the linear filter
  /// alone does. `WAV=1` writes each output to `test_data/private/aec_ab/<capture>_v<n>.wav`
  /// (n: place in `VARIANTS`), for `ai_denoiser::tests::capture_replay_report` with `AEC_WAV`.
  /// `VARIANTS="pregain=0;;open;erle=30,10 hf=0" cargo test --release -p gamersequalizer-mic-vst --lib capture_tuning_report -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn capture_tuning_report() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_data/private");
    let variants = std::env::var("VARIANTS").unwrap_or("pregain=0;;open;erle=30,10 hf=0".into());
    let only = std::env::var("CAP").unwrap_or_default();
    let Ok(dirs) = std::fs::read_dir(base.join("captures")) else { return eprintln!("no captures") };
    let mut dirs: Vec<_> = dirs.flatten().map(|d| d.path()).collect();
    dirs.sort();
    for dir in dirs {
      let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
      if !only.is_empty() && !only.split(',').any(|c| c == name) {
        continue;
      }
      let (Some(mic), Some(refs)) = (read_capture(&dir, "01_mic_raw.wav"), read_capture(&dir, "03_ref_aec.wav")) else {
        continue;
      };
      let heard = sort_windows(&mic, &refs);
      let count = |h: Heard| heard.iter().skip(COLD_WINDOWS).filter(|x| **x == h).count();
      eprintln!("\n{name}: {} voice windows, {} with the echo alone", count(Heard::Voice), count(Heard::EchoAlone));
      eprintln!("  {:<30} gain   echo   left  voice  >6dB >10dB  onset  4-8k   ERLE", "variant");
      for (place, variant) in variants.split(';').enumerate() {
        let gains = match variant.split_whitespace().find_map(|t| t.strip_prefix("pregain=")) {
          Some(db) => vec![db.parse().expect("pregain=<dB>"); mic.len().div_ceil(480)],
          None => capture_gains(&dir, mic.len()),
        };
        let mut aec = EchoCanceller3::new();
        aec.variant = variant.into();
        aec.rebuild();
        let (out, erle) = run_gained(&mut aec, &refs, &mic, &gains);
        let label = if variant.trim().is_empty() { "(as built)" } else { variant.trim() };
        eprintln!("  {label:<30} {:4.0} {}", aec.mic_gain_db(), window_report(&mic, &out, &erle, &heard));
        if std::env::var("WAV").is_ok() {
          let _ = std::fs::create_dir_all(base.join("aec_ab"));
          write_wav(&base.join("aec_ab").join(format!("{name}_v{place}.wav")), &out);
        }
      }
    }
  }

  /// Each 100 ms window of a capture by what Silero hears on the raw mic: a voice (over 0.5 in
  /// one of the chunks it spans), the echo alone (under 0.15 from 200 ms before to 200 ms after,
  /// so no word's tail counts as echo, while the reference is over -60 dBFS), or neither.
  fn sort_windows(mic: &[f32], refs: &[f32]) -> Vec<Heard> {
    let (mut vad, mut probs) = (silero_vad::Vad48k::new().expect("Silero weights"), Vec::new());
    mic.chunks(480).for_each(|b| vad.process(b, |p| probs.push(p)));
    let voice: Vec<f32> = (0..mic.len() / WINDOW)
      .map(|w| {
        let first = w * WINDOW / silero_vad::CHUNK_48K;
        (first..first + 4).filter_map(|c| probs.get(c)).fold(0.0f32, |a, b| a.max(*b))
      })
      .collect();
    (0..voice.len())
      .map(|w| {
        let around = &voice[w.saturating_sub(2)..(w + 3).min(voice.len())];
        let playing = energy_db(refs.get(w * WINDOW..(w + 1) * WINDOW).unwrap_or_default()) > -60.0;
        match voice[w] {
          p if p > 0.5 => Heard::Voice,
          _ if playing && around.iter().all(|p| *p < 0.15) => Heard::EchoAlone,
          _ => Heard::Other,
        }
      })
      .collect()
  }

  /// `capture_tuning_report`'s columns after the gain, for one AEC3 output (`erle`: per
  /// 480-sample block).
  fn window_report(mic: &[f32], out: &[f32], erle: &[f32], heard: &[Heard]) -> String {
    // The output lags the mic by AEC3's latency.
    let out = out.get(AEC3_LATENCY_SAMPLES..).unwrap_or_default();
    let (mic_hf, out_hf) = (high_pass(mic, 4_000.0), high_pass(out, 4_000.0));
    let level = |x: &[f32], w: usize| energy_db(x.get(w * WINDOW..(w + 1) * WINDOW).unwrap_or_default());
    let (mut removed, mut left, mut erle_sum) = (Vec::new(), Vec::new(), 0.0f32);
    let (mut voice, mut loss, mut over6, mut over10, mut hf_loss) = (0usize, 0.0f32, 0usize, 0usize, 0.0f32);
    let (mut onsets, mut onset_loss) = (0usize, 0.0f32);
    // `take`: the shifted output is short of the capture's last window.
    for (w, h) in heard.iter().enumerate().take(out.len() / WINDOW).skip(COLD_WINDOWS) {
      let lost = level(mic, w) - level(out, w);
      match h {
        Heard::EchoAlone => {
          removed.push(lost);
          left.push(level(out, w));
          erle_sum += erle.get(w * 10 + 9).copied().unwrap_or(0.0);
        }
        Heard::Voice => {
          voice += 1;
          loss += lost;
          over6 += usize::from(lost > 6.0);
          over10 += usize::from(lost > 10.0);
          hf_loss += level(&mic_hf, w) - level(&out_hf, w);
          if heard[w - 3..w].iter().all(|x| *x != Heard::Voice) {
            onsets += 1;
            onset_loss += lost;
          }
        }
        Heard::Other => {}
      }
    }
    let mean = |sum: f32, n: usize| sum / n.max(1) as f32;
    let share = |n: usize| n as f32 * 100.0 / voice.max(1) as f32;
    let alone = removed.len();
    removed.sort_by(f32::total_cmp);
    left.sort_by(f32::total_cmp);
    format!(
      "{:6.1} {:6.1} {:6.1} {:5.0} {:5.0} {:6.1} {:5.1} {:6.1}",
      removed.get(alone / 2).copied().unwrap_or(0.0),
      left.get(alone * 9 / 10).copied().unwrap_or(0.0),
      mean(loss, voice),
      share(over6),
      share(over10),
      mean(onset_loss, onsets),
      mean(hf_loss, voice),
      mean(erle_sum, alone)
    )
  }

  /// Double talk, `echo` + `voice` on the mic: dB of echo removed and the voice level change
  /// over `range`. The output is projected on the echo and on the voice (both delayed by AEC3's
  /// latency; noise and a tone are uncorrelated), so the suppressor shaping the voice a little
  /// doesn't read as leaked echo.
  fn split(far: &[f32], echo: &[f32], voice: &[f32], range: std::ops::Range<usize>) -> (f32, f32) {
    let mic: Vec<f32> = echo.iter().zip(voice).map(|(e, v)| e + v).collect();
    let out = run(&mut EchoCanceller3::new(), far, &mic, 480);
    (-projection_db(echo, &out, range.clone()), projection_db(voice, &out, range))
  }

  /// Level change (dB) of `x` on its way into `out`, over `range` of `out`: `out` projected on
  /// `x` delayed by AEC3's latency.
  fn projection_db(x: &[f32], out: &[f32], range: std::ops::Range<usize>) -> f32 {
    let (mut xo, mut xx) = (0.0f64, 0.0f64);
    for n in range {
      let d = delayed(x, n, AEC3_LATENCY_SAMPLES) as f64;
      xo += d * out[n] as f64;
      xx += d * d;
    }
    20.0 * ((xo / xx.max(1e-20)).abs() + 1e-10).log10() as f32
  }

  /// 16 s at the levels of a quiet mic: a music-like far end at -18 dBFS, its echo through a
  /// short room at `echo_db` on the mic, room noise at -76 dBFS, a voice at `voice_db` from 8
  /// to 12 s; the chain asks for `mic_gain_db`. Echo removed over 4-8 s, voice over 8-12 s.
  fn quiet_mic(echo_db: f32, voice_db: f32, mic_gain_db: f32) -> QuietMic {
    let len = 16 * 48_000;
    let far = at_level(&music_like(len, 7), -18.0);
    let echo = at_level(&convolve(&far, &room_response(400, PRE_DELAY_SAMPLES, 0.3, 17)), echo_db);
    let (alone, both) = (4 * 48_000..8 * 48_000, 8 * 48_000..12 * 48_000);
    let mut voice = vec![0.0f32; len];
    voice[both.clone()].copy_from_slice(&at_level(&voice_like(both.len(), 3), voice_db));
    let room = at_level(&noise(len, 9), -76.0);
    let mic: Vec<f32> = (0..len).map(|n| echo[n] + voice[n] + room[n]).collect();
    let mut aec = EchoCanceller3::new();
    aec.set_mic_gain_db(mic_gain_db);
    let out = run(&mut aec, &far, &mic, 480);
    QuietMic {
      echo_removed_db: energy_db(&mic[alone.clone()]) - energy_db(&out[alone]),
      voice_db: projection_db(&voice, &out, both.clone()),
      voice_hf_db: projection_db(&high_pass(&voice, 4_000.0), &high_pass(&out, 4_000.0), both),
    }
  }

  /// `x` scaled to an RMS level (dBFS).
  fn at_level(x: &[f32], db: f32) -> Vec<f32> {
    let gain = 10f32.powf((db - energy_db(x)) / 20.0);
    x.iter().map(|v| v * gain).collect()
  }

  /// Low-passed noise plus a little white noise, with a 2 Hz beat: music's spectrum tilt and
  /// dynamics, on all the time (a continuous far end is the hard case for the suppressor).
  fn music_like(len: usize, seed: u32) -> Vec<f32> {
    let mut low = 0.0f32;
    noise(len, seed)
      .iter()
      .enumerate()
      .map(|(n, &w)| {
        low = 0.97 * low + 0.03 * w;
        (low * 6.0 + w * 0.15) * (0.6 + 0.4 * (n as f32 * 2.0 * std::f32::consts::PI * 2.0 / 48_000.0).sin())
      })
      .collect()
  }

  /// `speech_like` with hiss over 4.5 kHz in the gaps between its syllables (s, sh): the band
  /// AEC3 took most from.
  fn voice_like(len: usize, seed: u32) -> Vec<f32> {
    let hiss = high_pass(&noise(len, seed + 100), 4_500.0);
    speech_like(len, seed)
      .iter()
      .zip(hiss)
      .enumerate()
      .map(|(n, (s, h))| {
        let gap = (-(n as f32 * 2.0 * std::f32::consts::PI * 4.0 / 48_000.0).sin()).max(0.0);
        s + h * 0.25 * gap * gap
      })
      .collect()
  }

  /// 24 dB / octave high-pass.
  fn high_pass(x: &[f32], frequency: f32) -> Vec<f32> {
    let band =
      BiquadBand { filter_type: FilterType::HighPass, frequency, q: 0.707, slope_db: 24, ..BiquadBand::default() };
    let mut filter = BiquadCascade::default();
    filter.set_bands(&[band], 48_000.0);
    let mut y = x.to_vec();
    filter.process(&mut y);
    y
  }

  /// Test-only changes to `tuning`, for trying a setting on the captures without a rebuild
  /// (`capture_tuning_report`). Space separated:
  /// `open` (suppressor transparent: the output is the linear filter's, or the mic while AEC3
  /// does not use it), `masks=<k>` (suppressor thresholds times k), `erle=<lf>,<hf>`
  /// (`erle.max_l` / `max_h`, defaults 4 / 1.5), `hf=<n>` (`bands_in_limiting_gain`, default 1,
  /// 0 = bins over 2 kHz no longer follow the 2 kHz bin), `len=<blocks>` (filter length,
  /// default 13 = 52 ms), `gain=<x>` (fallback echo gain, default 0.01). `pregain=` belongs
  /// to the report.
  pub(super) fn apply_variant(cfg: &mut EchoCanceller3Config, spec: &str, mic_gain: f32) {
    for token in spec.split_whitespace() {
      let (key, value) = token.split_once('=').unwrap_or((token, ""));
      let mut values = value.split(',').map(|v| v.parse::<f32>().unwrap_or_else(|_| panic!("AEC3 variant {token:?}")));
      let mut number = || values.next().unwrap_or_else(|| panic!("AEC3 variant {token:?}: value missing"));
      let tunings = [&mut cfg.suppressor.normal_tuning, &mut cfg.suppressor.nearend_tuning];
      match key {
        "pregain" => {}
        "open" => {
          for mask in tunings.into_iter().flat_map(|t| [&mut t.mask_lf, &mut t.mask_hf]) {
            (mask.enr_transparent, mask.enr_suppress) = (99.0, 100.0);
          }
        }
        "masks" => {
          let k = number();
          for mask in tunings.into_iter().flat_map(|t| [&mut t.mask_lf, &mut t.mask_hf]) {
            (mask.enr_transparent, mask.enr_suppress) = (mask.enr_transparent * k, mask.enr_suppress * k);
          }
        }
        "erle" => (cfg.erle.max_l, cfg.erle.max_h) = (number(), number()),
        "hf" => cfg.suppressor.high_frequency_suppression.bands_in_limiting_gain = number() as i32,
        "len" => {
          let blocks = number() as usize;
          (cfg.filter.refined.length_blocks, cfg.filter.coarse.length_blocks) = (blocks, blocks);
        }
        "gain" => cfg.ep_strength.default_gain = number() * mic_gain,
        _ => panic!("unknown AEC3 variant {token:?}"),
      }
    }
  }

  fn read_capture(dir: &Path, name: &str) -> Option<Vec<f32>> {
    Some(hound::WavReader::open(dir.join(name)).ok()?.samples::<f32>().map_while(Result::ok).collect())
  }

  /// What the chain hands to `set_mic_gain_db` at each 480-sample block of a capture, from its
  /// `vars.tsv` as `plugin.rs` does (`VoiceLeveler::gain_to_default_db`): the leveler's default
  /// target less the learned speech level and the input gain. 0 without the file or its
  /// columns, before a level is learned, or with the leveler off.
  fn capture_gains(dir: &Path, samples: usize) -> Vec<f32> {
    let vars = std::fs::read_to_string(dir.join("vars.tsv")).unwrap_or_default();
    let mut lines = vars.lines();
    let header: Vec<&str> = lines.next().unwrap_or_default().split('\t').collect();
    let column = |name: &str| header.iter().position(|h| *h == name);
    let (frame, level, input, on) =
      (column("frame"), column("leveler_level_db"), column("input_gain_db"), column("leveler_on"));
    let default_target = MIC_LEVELER_DEFAULT_TARGET_DB_X10 as f32 / 10.0;
    let mut rows = lines
      .map(|line| {
        let cell = |at: Option<usize>| at.and_then(|at| line.split('\t').nth(at)).and_then(|c| c.parse::<f32>().ok());
        // NaN: no level learned yet.
        let wanted =
          cell(level).filter(|l| l.is_finite()).map_or(0.0, |l| default_target - l - cell(input).unwrap_or(0.0));
        (cell(frame).unwrap_or(0.0) as usize, if cell(on) == Some(0.0) { 0.0 } else { wanted })
      })
      .peekable();
    let mut now = 0.0;
    (0..samples.div_ceil(480))
      .map(|block| {
        while let Some(row) = rows.next_if(|row| row.0 <= block * 480) {
          now = row.1;
        }
        now
      })
      .collect()
  }

  /// `run` in 480-sample blocks with the mic gain the chain asks for in each (`gains`, dB).
  /// Also AEC3's own ERLE estimate after each block.
  fn run_gained(aec: &mut EchoCanceller3, refs: &[f32], mic: &[f32], gains: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let (mut out, mut erle) = (Vec::with_capacity(mic.len()), Vec::new());
    for (k, (r, m)) in refs.chunks(480).zip(mic.chunks(480)).enumerate() {
      aec.set_mic_gain_db(gains.get(k).copied().unwrap_or(0.0));
      let mut b = m.to_vec();
      aec.process_block(r, &mut b);
      out.extend_from_slice(&b);
      erle.push(aec.erle_db());
    }
    (out, erle)
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
    let spec = hound::WavSpec {
      channels: 1,
      sample_rate: 48_000,
      bits_per_sample: 32,
      sample_format: hound::SampleFormat::Float,
    };
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
