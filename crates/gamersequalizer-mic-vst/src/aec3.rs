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
//!
//! Echo strength: how much quieter than the reference its echo reaches the mic depends on the
//! speakers, their volume and the mic (headset: no echo, laptop: almost as loud as the
//! reference). It is measured next to AEC3 (`EchoMeter`), and AEC3's fallback echo estimate
//! follows a strong echo instead of assuming one 40 dB down.

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
/// AEC3's fallback echo estimate while it cannot use its linear filter (a start, a reset, an
/// echo path change): the reference this many dB down (`ep_strength.default_gain`, see
/// `tuning`). The lowest it goes: what the captures were tuned with...
const FALLBACK_ECHO_DB: f32 = -40.0;
/// ...and with a stronger echo measured, this far under it: -40 dB was 3 dB under the strongest
/// echo of those captures and removed it, 13 dB under leaked (milestone 42).
const FALLBACK_UNDER_ECHO_DB: f32 = 3.0;
/// Echo strength is measured in windows of this many frames (100 ms)...
const ECHO_WINDOW_FRAMES: u32 = 10;
/// ...in which the reference is this loud (mean square; -30 dBFS).
const REF_LOUD_POWER: f32 = 1e-3;
/// The last such windows kept (12.8 s), and how many it takes for a first estimate (5 s).
const ECHO_WINDOWS: usize = 128;
const ECHO_MIN_WINDOWS: usize = 50;
/// An echo is confirmed when the mic's level follows the reference's in the windows quietest
/// for their reference: correlation of the levels at least this, a slope near 1...
const ECHO_FIT_MIN_CORRELATION: f32 = 0.9;
const ECHO_FIT_SLOPE: std::ops::RangeInclusive<f32> = 0.7..=1.3;
/// ...and a reference whose level moves at least this much in them (standard deviation, dB):
/// a level that stands still shows nothing.
const ECHO_FIT_MIN_REF_SPREAD_DB: f32 = 2.0;
/// A confirmed strength comes down when the bound falls this far under it (the speakers were
/// turned down, headphones went on).
const ECHO_BOUND_UNDER_DB: f32 = 3.0;
/// The reported echo strength moves only by this much (dB), so a log shows steps, not jitter.
const ECHO_REPORT_STEP_DB: f32 = 2.0;

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
  echo: EchoMeter,
  /// Fallback echo estimate AEC3 was built for (dB re the reference) and the step asked for.
  fallback_db: f32,
  wanted_fallback_db: f32,
  /// Test-only overrides of `tuning` (`tests::apply_variant`).
  #[cfg(test)]
  variant: String,
}

/// Echo strength: how many dB under the reference its echo reaches the raw mic. Per 100 ms
/// window with a loud reference: mic level - reference level.
///
/// The user's voice and the mic's noise only ever add to a window, so a low quartile of the
/// last windows is a bound: the echo is no stronger than that. It is not the echo: on the
/// captures a voice over the music read as an echo 12-30 dB under it (milestone 46: headphones
/// with no echo at all "-31 dB", speech over a -50 dB echo "-33 dB").
///
/// An echo is told from a voice by following the reference: where the echo is what the mic
/// hears, the mic's level goes up and down with the reference's, window by window; a voice or
/// a noise does not. Only then is the strength confirmed, and only a confirmed one moves
/// AEC3's fallback. Not confirmed, the meter says "a bound" and AEC3 keeps its lowest
/// fallback, as before there was a meter.
struct EchoMeter {
  /// The window being filled: reference energy, mic energy, frames.
  window: (f32, f32, u32),
  /// (reference level, mic level) in dB of the last loud-reference windows.
  loud: [(f32, f32); ECHO_WINDOWS],
  loud_count: usize,
  /// The strength last confirmed or remembered, brought down by a lower bound since.
  confirmed: Option<f32>,
  /// What is reported (see `ECHO_REPORT_STEP_DB`): the confirmed strength, else the bound.
  shown: Option<(f32, bool)>,
  /// Correlation and slope of the last fit: for the log, where they show how near a real
  /// room comes to `ECHO_FIT_MIN_CORRELATION`.
  fit: (f32, f32),
  /// `ECHO_FIT_MIN_CORRELATION`; the capture report tries others.
  min_correlation: f32,
}

impl EchoCanceller3 {
  pub fn new() -> Self {
    Self {
      apm: build_apm(tuning(1.0, 10f32.powf(FALLBACK_ECHO_DB / 20.0))),
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
      echo: EchoMeter::new(),
      fallback_db: FALLBACK_ECHO_DB,
      wanted_fallback_db: FALLBACK_ECHO_DB,
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
    self.wanted_gain_db = stepped(want, self.wanted_gain_db);
  }

  /// Mic gain AEC3 runs with right now (dB).
  pub fn mic_gain_db(&self) -> f32 {
    self.mic_gain_db
  }

  /// Echo strength: how many dB under the reference its echo reaches the mic, and whether that
  /// is only an upper bound (no echo was seen following the reference: the user's voice or the
  /// mic's noise may be all there is). None until 5 s of loud reference or a remembered value.
  /// Moves in steps of 2 dB or more.
  pub fn echo_strength_db(&self) -> Option<(f32, bool)> {
    self.echo.shown
  }

  /// How the mic's level followed the reference's in the last fit: correlation and slope
  /// (an echo is confirmed from 0.9 and a slope of 0.7-1.3). (0, 0) before the first.
  pub fn echo_fit(&self) -> (f32, f32) {
    self.echo.fit
  }

  /// Echo strength remembered for this mic and output: used until one is confirmed here, or
  /// the bound falls under it.
  pub fn seed_echo_strength_db(&mut self, db: f32) {
    if db.is_finite() && self.echo.loud_count < ECHO_MIN_WINDOWS {
      self.echo.confirmed = Some(db);
      self.echo.shown = Some((db, false));
    }
  }

  /// Other speakers: what was measured is theirs no more.
  pub fn forget_echo_strength(&mut self) {
    self.echo = EchoMeter::new();
    self.wanted_fallback_db = FALLBACK_ECHO_DB;
  }

  /// Fallback echo estimate AEC3 runs with right now: the reference this many dB down.
  pub fn fallback_echo_db(&self) -> f32 {
    self.fallback_db
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
    self.echo.push(&self.in_ref, &self.in_mic);
    if let Some((echo_db, bound)) = self.echo.shown {
      // A bound may be the user's voice: no reason to leave the lowest estimate.
      let want = if bound { FALLBACK_ECHO_DB } else { (echo_db - FALLBACK_UNDER_ECHO_DB).clamp(FALLBACK_ECHO_DB, 0.0) };
      // `min`: the steps count from -40 dB and would end at +2.
      let steps = stepped(want - FALLBACK_ECHO_DB, self.wanted_fallback_db - FALLBACK_ECHO_DB);
      self.wanted_fallback_db = (steps + FALLBACK_ECHO_DB).min(0.0);
    }
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

  /// Rebuild AEC3 for the wanted mic gain and fallback echo estimate once it is due. `peak`:
  /// this frame's mic peak.
  fn follow_wanted_gain(&mut self, peak: f32) {
    let last_peak = std::mem::replace(&mut self.last_peak, peak);
    if self.wanted_gain_db == self.mic_gain_db && self.wanted_fallback_db == self.fallback_db {
      (self.wanted_frames, self.wanted_peak) = (0, 0.0);
      return;
    }
    self.wanted_frames += 1;
    self.wanted_peak = self.wanted_peak.max(peak);
    // The old AEC3 still holds the end of the last frame, which is lost: that one counts too.
    let pause =
      peak.max(last_peak) <= self.wanted_peak * MIC_GAIN_PAUSE_RATIO || self.wanted_frames >= 3 * MIC_GAIN_LATCH_FRAMES;
    if self.free_frames > 0 || (self.wanted_frames >= MIC_GAIN_LATCH_FRAMES && pause) {
      log::info!(
        "[AEC3] mic gain {} -> {} dB, fallback echo {} -> {} dB",
        self.mic_gain_db,
        self.wanted_gain_db,
        self.fallback_db,
        self.wanted_fallback_db
      );
      self.rebuild();
    }
  }

  /// New AEC3 for the wanted mic gain and fallback echo estimate. Allocates on the audio thread
  /// like `reset`: at a stream's start, and when the learned speech level or the measured echo
  /// strength moves by a step (once per 2 s at most).
  fn rebuild(&mut self) {
    self.mic_gain_db = self.wanted_gain_db;
    self.mic_gain = 10f32.powf(self.mic_gain_db / 20.0);
    self.fallback_db = self.wanted_fallback_db;
    #[allow(unused_mut)] // only the tests change it
    let mut config = tuning(self.mic_gain, 10f32.powf(self.fallback_db / 20.0));
    #[cfg(test)]
    tests::apply_variant(&mut config, &self.variant, self.mic_gain);
    self.apm = build_apm(config);
    self.erle_db = 0.0;
    (self.wanted_frames, self.wanted_peak) = (0, 0.0);
  }
}

impl EchoMeter {
  fn new() -> Self {
    Self {
      window: (0.0, 0.0, 0),
      loud: [(0.0, 0.0); ECHO_WINDOWS],
      loud_count: 0,
      confirmed: None,
      shown: None,
      fit: (0.0, 0.0),
      min_correlation: ECHO_FIT_MIN_CORRELATION,
    }
  }

  /// One frame of the reference as AEC3 gets it and of the raw mic.
  fn push(&mut self, refs: &[f32], mic: &[f32]) {
    let energy = |x: &[f32]| x.iter().map(|s| s * s).sum::<f32>();
    self.window = (self.window.0 + energy(refs), self.window.1 + energy(mic), self.window.2 + 1);
    if self.window.2 < ECHO_WINDOW_FRAMES {
      return;
    }
    let samples = (self.window.2 as usize * FRAME) as f32;
    let (ref_power, mic_power) = (self.window.0 / samples, self.window.1 / samples);
    self.window = (0.0, 0.0, 0);
    if ref_power >= REF_LOUD_POWER {
      if let Some(slot) = self.loud.get_mut(self.loud_count % ECHO_WINDOWS) {
        *slot = (10.0 * ref_power.log10(), 10.0 * mic_power.max(1e-12).log10());
      }
      self.loud_count += 1;
      self.estimate();
    }
  }

  fn estimate(&mut self) {
    let filled = self.loud_count.min(ECHO_WINDOWS);
    if filled < ECHO_MIN_WINDOWS {
      return;
    }
    // (mic level - reference level, reference level), the quietest for its reference first.
    let mut sorted = self.loud.map(|(ref_db, mic_db)| (mic_db - ref_db, ref_db));
    let sorted = sorted.get_mut(..filled).unwrap_or_default();
    sorted.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let Some(&(bound, _)) = sorted.get(filled / 4) else { return };

    // Does the mic follow the reference in the quieter half (the windows a voice is least in)?
    let lower = sorted.get(..filled / 2).unwrap_or_default();
    let n = lower.len() as f32;
    let ref_mean = lower.iter().map(|w| w.1).sum::<f32>() / n;
    let mic_mean = lower.iter().map(|w| w.0 + w.1).sum::<f32>() / n;
    let (mut ref_var, mut mic_var, mut cov) = (0.0f32, 0.0f32, 0.0f32);
    for &(ratio, ref_db) in lower {
      let (r, m) = (ref_db - ref_mean, ratio + ref_db - mic_mean);
      (ref_var, mic_var, cov) = (ref_var + r * r / n, mic_var + m * m / n, cov + r * m / n);
    }
    let correlation = cov / (ref_var * mic_var).sqrt().max(1e-9);
    let slope = cov / ref_var.max(1e-9);
    self.fit = (correlation, slope);
    let follows = ref_var.sqrt() >= ECHO_FIT_MIN_REF_SPREAD_DB
      && correlation >= self.min_correlation
      && ECHO_FIT_SLOPE.contains(&slope);
    if follows || self.confirmed.is_some_and(|c| bound < c - ECHO_BOUND_UNDER_DB) {
      self.confirmed = Some(bound);
    }
    let now = self.confirmed.map_or((bound, true), |c| (c, false));
    if self.shown.is_none_or(|was| (was.0 - now.0).abs() >= ECHO_REPORT_STEP_DB || was.1 != now.1) {
      self.shown = Some(now);
    }
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
/// `fallback_gain`: that fallback at mic gain 0 dB, 0.01 unless a stronger echo was measured
/// (`FALLBACK_ECHO_DB`). Synthetic, echo 20 dB under the reference: with 0.01 the first 0.4 s
/// after a start pass whole (-38.0 -> -38.8 dBFS), with 0.1 they come out at -58.9 dBFS.
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
fn tuning(mic_gain: f32, fallback_gain: f32) -> EchoCanceller3Config {
  let mut cfg = EchoCanceller3Config::default();
  cfg.ep_strength.default_gain = fallback_gain * mic_gain;
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

/// `current`, unless `want` is half a step plus the hysteresis away from it: then the step
/// nearest to `want` (dB, steps counted from 0).
fn stepped(want: f32, current: f32) -> f32 {
  if (want - current).abs() >= MIC_GAIN_STEP_DB / 2.0 + MIC_GAIN_HYSTERESIS_DB {
    (want / MIC_GAIN_STEP_DB).round() * MIC_GAIN_STEP_DB
  } else {
    current
  }
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
    /// Echo strength and AEC3's fallback echo estimate at the end.
    echo_strength_db: Option<(f32, bool)>,
    fallback_db: f32,
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
    // The strength measured agrees with the one remembered: no step, no rebuild on the way.
    assert_eq!((plain.fallback_db, gained.fallback_db), (-22.0, -22.0), "{plain:?} / {gained:?}");
    assert!(plain.echo_strength_db.is_some_and(|(db, bound)| !bound && (db + 22.0).abs() < 3.0), "{plain:?}");
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

  // Echo strength (2026-11-10): no capture has an echo stronger than 37 dB under the reference.

  /// The meter finds how far under the reference the echo is, past the user's voice (louder than
  /// the echo, in 40 % of the windows) and the mic's noise, with music that starts after 2 s or
  /// plays from the start (game audio never pauses). Without an echo that follows the
  /// reference it reports a bound, whatever the voice does: milestone 46's captures, where a
  /// voice over music on headphones read as an echo 31 dB under the reference and speech over
  /// a weak echo as one 33 dB under. Only a confirmed strong echo moves AEC3's fallback
  /// estimate off -40 dB, and never over 0 dB.
  #[test]
  fn test_echo_strength_is_measured_past_voice_and_noise() {
    let some: fn(usize) -> bool = |sec| sec % 5 < 2;
    let always: fn(usize) -> bool = |_| true;
    let late: fn(usize) -> bool = |sec| sec >= 8;
    // (echo under the reference, music starts at, when the user talks, strength expected or
    // None for a bound, fallback in the end)
    let scenes = [
      (-20.0f32, 2usize, some, Some(-20.0f32), -22.0f32),
      (-50.0, 2, some, Some(-50.0), -40.0),
      (-80.0, 2, some, None, -40.0),
      (-20.0, 0, some, Some(-20.0), -22.0),
      (-50.0, 0, some, Some(-50.0), -40.0),
      (-200.0, 0, some, None, -40.0),
      (5.0, 2, some, Some(5.0), 0.0),
      // Headphones, talking all the time over the music: no echo to find.
      (-200.0, 0, always, None, -40.0),
      // A weak echo, confirmed in the 3 s before the voice starts: the voice, 20 dB louder
      // and never pausing, does not move it.
      (-50.0, 0, always, Some(-50.0), -40.0),
      // A strong echo, confirmed before the user starts and never stops talking: it stays.
      (-20.0, 0, late, Some(-20.0), -22.0),
    ];
    for (echo_db, music_at, talking, expected, fallback_db) in scenes {
      let (far, mic) = echo_scene(echo_db, 20, music_at, talking);
      let mut aec = EchoCanceller3::new();
      run(&mut aec, &far, &mic, 480);
      let (measured, bound) = aec.echo_strength_db().expect("an estimate after 18 s of music");
      let (correlation, slope) = aec.echo.fit;
      eprintln!(
        "echo {echo_db} dB, music from {music_at} s: {measured:.1} dB (bound: {bound}), fit {correlation:.2} x {slope:.2}"
      );
      match expected {
        Some(db) => assert!(!bound && (measured - db).abs() < 3.0, "{echo_db} dB read as {measured} (bound: {bound})"),
        None => assert!(bound, "{echo_db} dB read as {measured}: no echo follows the reference here"),
      }
      assert_eq!(aec.fallback_echo_db(), fallback_db, "echo {echo_db} dB, music from {music_at} s");
    }
  }

  /// A laptop-like echo, 20 dB under the reference: with the fixed -40 dB fallback the first
  /// 0.4 s of a stream pass whole (AEC3 has no filter yet); with the strength remembered for
  /// these speakers they are suppressed from the first frame.
  #[test]
  fn test_remembered_strong_echo_is_suppressed_from_the_start() {
    let (far, mic) = echo_scene(-20.0, 3, 2, |_| false);
    // The music starts at 2 s.
    let start = 2 * 48_000 + 4_800 + AEC3_LATENCY_SAMPLES..2 * 48_000 + 24_000;
    let mut plain = EchoCanceller3::new();
    let plain_db = energy_db(&run(&mut plain, &far, &mic, 480)[start.clone()]);
    let mut seeded = EchoCanceller3::new();
    seeded.seed_echo_strength_db(-20.0);
    let seeded_db = energy_db(&run(&mut seeded, &far, &mic, 480)[start.clone()]);
    eprintln!(
      "strong echo, first 0.4 s: mic {:.1}, out {plain_db:.1}, remembered {seeded_db:.1} dBFS",
      energy_db(&mic[start])
    );
    assert_eq!(seeded.fallback_echo_db(), -22.0);
    assert!(seeded_db < plain_db - 10.0, "remembered {seeded_db} dBFS, not remembered {plain_db} dBFS");
    seeded.forget_echo_strength();
    assert_eq!(seeded.echo_strength_db(), None, "other speakers: nothing known");
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
  /// mic gain at the end; the echo strength measured (dB under the reference, `<`: only a
  /// bound, no echo was seen following the reference) and the fallback echo estimate at the end;
  /// while the echo is alone: dB removed (median window) and the output
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
      eprintln!("  {:<30} gain  strength fallb   echo   left  voice  >6dB >10dB  onset  4-8k   ERLE", "variant");
      for (place, variant) in variants.split(';').enumerate() {
        let gains = match variant.split_whitespace().find_map(|t| t.strip_prefix("pregain=")) {
          Some(db) => vec![db.parse().expect("pregain=<dB>"); mic.len().div_ceil(480)],
          None => capture_gains(&dir, mic.len()),
        };
        let mut aec = EchoCanceller3::new();
        aec.variant = variant.into();
        aec.rebuild();
        let (out, erle, _) = run_gained(&mut aec, &refs, &mic, &gains);
        let label = if variant.trim().is_empty() { "(as built)" } else { variant.trim() };
        let strength = match aec.echo_strength_db() {
          Some((db, true)) => format!("<{db:.0}"),
          Some((db, false)) => format!("{db:.0}"),
          None => "-".into(),
        };
        let report = window_report(&mic, &out, &erle, &heard);
        eprintln!("  {label:<30} {:4.0}  {strength:>8} {:5.0} {report}", aec.mic_gain_db(), aec.fallback_echo_db());
        if std::env::var("WAV").is_ok() {
          let _ = std::fs::create_dir_all(base.join("aec_ab"));
          write_wav(&base.join("aec_ab").join(format!("{name}_v{place}.wav")), &out);
        }
      }
    }
  }

  /// Echo strength on the debug captures: what the meter reads, what follows from it, and how
  /// it moves over a capture. Each capture (`CAP=a,b`: only those) once per scene (`SCENES`,
  /// `;` between them). A scene is a list of `apply_variant` keys and of:
  /// - `micscale=<dB>`: the raw mic that much louder, so the echo is that much stronger against
  ///   the reference, as with speakers close to the mic (no capture has an echo stronger than
  ///   37 dB under the reference). The mic gain asked for goes down by as much, as the leveler
  ///   would have learned the louder voice.
  /// - `seed=<dB>` or `seed=auto`: the strength a session remembers from the one before (the
  ///   daemon's cache); `auto` is what the scene before this one measured.
  /// - `fit=<r>`: the correlation an echo is confirmed from, instead of 0.9: which captures
  ///   would confirm at a lower one, and which would do so wrongly.
  ///
  /// Columns: `meter`: the strength at the end (dB under the reference, `<`: only a bound, no
  /// echo was seen following the reference); `fit`: correlation x slope of the mic's level
  /// against the reference's at the end (confirmed from 0.90 x 0.7..1.3);
  /// `check`: the same measured another way, the median of mic level - reference level over the
  /// windows Silero finds the echo alone in while the reference is over -30 dBFS (the mic's
  /// noise is not taken off: for a weak echo this reads higher than the meter); `fallb`: the
  /// fallback echo estimate at the end; `start`: dB removed from the echo-alone windows in the
  /// first 0.1-0.5 s of loud reference, where AEC3 has no filter yet and the fallback is all
  /// it has; the rest as in `capture_tuning_report`, from 3 s on. Under each line the trace: when the reported
  /// strength, the fallback and the mic gain changed, as the debug log would show it.
  /// `SCENES=";micscale=20;micscale=20 seed=auto" cargo test --release -p gamersequalizer-mic-vst --lib capture_echo_report -- --ignored --nocapture`
  #[test]
  #[ignore = "needs private recordings"]
  fn capture_echo_report() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_data/private");
    let scenes = std::env::var("SCENES").unwrap_or(";micscale=20;micscale=20 seed=auto".into());
    let only = std::env::var("CAP").unwrap_or_default();
    let Ok(dirs) = std::fs::read_dir(base.join("captures")) else { return eprintln!("no captures") };
    let mut dirs: Vec<_> = dirs.flatten().map(|d| d.path()).collect();
    dirs.sort();
    let cell = |v: Option<f32>| v.map_or("-".to_string(), |v| format!("{v:.1}"));
    for dir in dirs {
      let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
      if !only.is_empty() && !only.split(',').any(|c| c == name) {
        continue;
      }
      let (Some(mic), Some(refs)) = (read_capture(&dir, "01_mic_raw.wav"), read_capture(&dir, "03_ref_aec.wav")) else {
        continue;
      };
      let heard = sort_windows(&mic, &refs);
      let level = |x: &[f32], w: usize| energy_db(x.get(w * WINDOW..(w + 1) * WINDOW).unwrap_or_default());
      let alone = |from: usize, to: usize| (from..to.min(heard.len())).filter(|w| heard[*w] == Heard::EchoAlone);
      let loud_alone = alone(COLD_WINDOWS, heard.len()).filter(|w| level(&refs, *w) > -30.0);
      let check = median(loud_alone.map(|w| level(&mic, w) - level(&refs, w)).collect());
      let music_at = (0..heard.len()).find(|w| level(&refs, *w) > -30.0).unwrap_or(heard.len());
      eprintln!("\n{name}: {} windows with the echo alone", alone(COLD_WINDOWS, heard.len()).count());
      eprintln!(
        "  {:<28} gain  meter        fit  check fallb  start   echo   left  voice  >6dB >10dB  onset  4-8k   ERLE",
        "scene"
      );
      let mut measured_before = None;
      for scene in scenes.split(';') {
        let key = |name: &str| scene.split_whitespace().find_map(|t| t.strip_prefix(name));
        let scale_db: f32 = key("micscale=").map_or(0.0, |v| v.parse().expect("micscale=<dB>"));
        let scaled: Vec<f32> = mic.iter().map(|s| s * 10f32.powf(scale_db / 20.0)).collect();
        let gains: Vec<f32> = capture_gains(&dir, mic.len()).iter().map(|g| g - scale_db).collect();
        let mut aec = EchoCanceller3::new();
        aec.variant = scene.into();
        aec.rebuild();
        if let Some(r) = key("fit=") {
          aec.echo.min_correlation = r.parse().expect("fit=<correlation>");
        }
        match key("seed=") {
          Some("auto") => measured_before.into_iter().for_each(|db| aec.seed_echo_strength_db(db)),
          Some(db) => aec.seed_echo_strength_db(db.parse().expect("seed=<dB>|auto")),
          None => {}
        }
        let (out, erle, trace) = run_gained(&mut aec, &refs, &scaled, &gains);
        let late = out.get(AEC3_LATENCY_SAMPLES..).unwrap_or_default();
        let start = median(alone(music_at + 1, music_at + 5).map(|w| level(&scaled, w) - level(late, w)).collect());
        let meter = match aec.echo_strength_db() {
          Some((db, true)) => format!("<{db:.1}"),
          Some((db, false)) => format!("{db:.1}"),
          None => "-".into(),
        };
        measured_before = aec.echo_strength_db().filter(|s| !s.1).map(|s| s.0);
        eprintln!(
          "  {:<28} {:4.0} {meter:>6} {:>10} {:>6} {:5.0} {:>6} {}",
          if scene.trim().is_empty() { "(as recorded)" } else { scene.trim() },
          aec.mic_gain_db(),
          format!("{:.2}x{:.2}", aec.echo.fit.0, aec.echo.fit.1),
          cell(check.map(|c| c + scale_db)),
          aec.fallback_echo_db(),
          cell(start),
          window_report(&scaled, &out, &erle, &heard)
        );
        let clipped = scaled.iter().filter(|s| s.abs() > 1.0).count();
        if clipped > 0 {
          eprintln!("    {clipped} samples of the scaled mic are over full scale: use a smaller micscale");
        }
        eprintln!("    trace: {}", if trace.is_empty() { "nothing changed".into() } else { trace.join(", ") });
      }
    }
  }

  /// The middle value; None for none.
  fn median(mut values: Vec<f32>) -> Option<f32> {
    values.sort_by(f32::total_cmp);
    values.get(values.len() / 2).copied()
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
  /// to 12 s; the chain asks for `mic_gain_db` and remembers the echo strength. Echo removed
  /// over 4-8 s, voice over 8-12 s.
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
    // As in every session after the first on these speakers: without it a strong echo
    // rebuilds AEC3 in the middle of the measurement.
    aec.seed_echo_strength_db(echo_db + 18.0);
    let out = run(&mut aec, &far, &mic, 480);
    QuietMic {
      echo_removed_db: energy_db(&mic[alone.clone()]) - energy_db(&out[alone]),
      voice_db: projection_db(&voice, &out, both.clone()),
      voice_hf_db: projection_db(&high_pass(&voice, 4_000.0), &high_pass(&out, 4_000.0), both),
      echo_strength_db: aec.echo_strength_db(),
      fallback_db: aec.fallback_echo_db(),
    }
  }

  /// `secs` of a mic: room noise (-76 dBFS), from `music_at` s music (-18 dBFS) whose echo is
  /// `echo_db` under it, and from 3 s a voice at -30 dBFS in the seconds `talking` says.
  /// (reference, mic)
  fn echo_scene(echo_db: f32, secs: usize, music_at: usize, talking: fn(usize) -> bool) -> (Vec<f32>, Vec<f32>) {
    let len = secs * 48_000;
    let music = at_level(&music_like(len, 7), -18.0);
    let far: Vec<f32> = music.iter().enumerate().map(|(n, &m)| if n < music_at * 48_000 { 0.0 } else { m }).collect();
    let (voice, room) = (at_level(&voice_like(len, 3), -30.0), at_level(&noise(len, 9), -76.0));
    let gain = 10f32.powf(echo_db / 20.0);
    let mic = (0..len)
      .map(|n| {
        let talking = n >= 3 * 48_000 && talking(n / 48_000);
        delayed(&far, n, PRE_DELAY_SAMPLES) * gain + room[n] + if talking { voice[n] } else { 0.0 }
      })
      .collect();
    (far, mic)
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
  /// default 13 = 52 ms), `gain=<x>` (fallback echo gain, default 0.01). `pregain=`,
  /// `micscale=` and `seed=` belong to the reports.
  pub(super) fn apply_variant(cfg: &mut EchoCanceller3Config, spec: &str, mic_gain: f32) {
    for token in spec.split_whitespace() {
      let (key, value) = token.split_once('=').unwrap_or((token, ""));
      let mut values = value.split(',').map(|v| v.parse::<f32>().unwrap_or_else(|_| panic!("AEC3 variant {token:?}")));
      let mut number = || values.next().unwrap_or_else(|| panic!("AEC3 variant {token:?}: value missing"));
      let tunings = [&mut cfg.suppressor.normal_tuning, &mut cfg.suppressor.nearend_tuning];
      match key {
        "pregain" | "micscale" | "seed" | "fit" => {}
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
  /// Also AEC3's own ERLE estimate after each block, and a trace: when the reported echo
  /// strength (with the fit that confirmed it: correlation x slope of the mic's level against
  /// the reference's), the fallback echo estimate and the mic gain changed.
  fn run_gained(
    aec: &mut EchoCanceller3,
    refs: &[f32],
    mic: &[f32],
    gains: &[f32],
  ) -> (Vec<f32>, Vec<f32>, Vec<String>) {
    let (mut out, mut erle, mut trace) = (Vec::with_capacity(mic.len()), Vec::new(), Vec::new());
    let state = |aec: &EchoCanceller3| (aec.echo_strength_db(), aec.fallback_echo_db(), aec.mic_gain_db());
    let mut last = state(aec);
    for (k, (r, m)) in refs.chunks(480).zip(mic.chunks(480)).enumerate() {
      aec.set_mic_gain_db(gains.get(k).copied().unwrap_or(0.0));
      let mut b = m.to_vec();
      aec.process_block(r, &mut b);
      out.extend_from_slice(&b);
      erle.push(aec.erle_db());
      let now = state(aec);
      let at = (k + 1) as f32 / 100.0;
      let (correlation, slope) = aec.echo.fit;
      match now.0.filter(|_| now.0 != last.0) {
        Some((db, true)) => trace.push(format!("{at:.1} s no echo confirmed (at most {db:.1})")),
        Some((db, false)) => trace.push(format!("{at:.1} s echo {db:.1} (fit {correlation:.2} x {slope:.2})")),
        None => {}
      }
      if now.1 != last.1 {
        trace.push(format!("{at:.1} s fallback {} -> {}", last.1, now.1));
      }
      if now.2 != last.2 {
        trace.push(format!("{at:.1} s mic gain {} -> {}", last.2, now.2));
      }
      last = now;
    }
    (out, erle, trace)
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
