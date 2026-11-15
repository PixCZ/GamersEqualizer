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
//!
//! Stereo reference: each speaker's echo has its own path to the mic (distance, level, colour),
//! and one filter on the mono mix can only follow them while left and right play the same.
//! While they differ AEC3 gets both channels and learns a filter for each (`mix_reference`);
//! with the same sound on both it runs on their mix, exactly as it did before there was a choice.

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
/// A reference frame is stereo when what its mono mix loses, (L - R) / 2, is over this share of
/// the channels' mean power (-30 dB). Under it a mono filter's error is below what AEC3 trusts
/// its linear filter for anyway (`erle.max_l`).
const STEREO_SIDE_RATIO: f32 = 1e-3;
/// Mean channel power (-60 dBFS) under which a frame says nothing: a pause in what plays.
const STEREO_MIN_POWER: f32 = 1e-6;
/// Stereo frames in a row before AEC3 is asked for both channels (1 s: not for a notification
/// sound), and mono frames in a row before it goes back to their mix (5 min, AEC3's own timeout
/// for its stereo mode). Pauses count for neither.
const STEREO_ON_FRAMES: u32 = 100;
const STEREO_OFF_FRAMES: u32 = 30_000;

pub struct EchoCanceller3 {
  apm: AudioProcessing,
  /// The reference frame's channels and their mono mix: AEC3 gets the mix unless `stereo`, the
  /// echo meter always.
  in_left: Vec<f32>,
  in_right: Vec<f32>,
  in_ref: Vec<f32>,
  in_mic: Vec<f32>,
  in_len: usize,
  out_frame: Vec<f32>,
  render_out: [Vec<f32>; 2],
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
  /// AEC3 was built for both reference channels, and whether it should be (`mix_reference`).
  stereo: bool,
  wanted_stereo: bool,
  /// Stereo frames in a row, mono frames in a row.
  stereo_run: u32,
  mono_run: u32,
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
      apm: build_apm(tuning(1.0, 10f32.powf(FALLBACK_ECHO_DB / 20.0), false), false),
      in_left: vec![0.0; FRAME],
      in_right: vec![0.0; FRAME],
      in_ref: vec![0.0; FRAME],
      in_mic: vec![0.0; FRAME],
      in_len: 0,
      out_frame: vec![0.0; FRAME],
      render_out: [vec![0.0; FRAME], vec![0.0; FRAME]],
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
      stereo: false,
      wanted_stereo: false,
      stereo_run: 0,
      mono_run: 0,
      #[cfg(test)]
      variant: String::new(),
    }
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

  /// AEC3 runs on both reference channels right now (a filter each), not on their mix.
  pub fn is_stereo(&self) -> bool {
    self.stereo
  }

  /// `process_block_stereo` with a mono reference.
  pub fn process_block(&mut self, ref_block: &[f32], mic_block: &mut [f32]) {
    self.process_block_stereo(ref_block, ref_block, mic_block);
  }

  /// Process a block in place. The reference's two channels must be delay-aligned to
  /// `mic_block`.
  pub fn process_block_stereo(&mut self, ref_left: &[f32], ref_right: &[f32], mic_block: &mut [f32]) {
    let n = mic_block.len().min(ref_left.len()).min(ref_right.len());
    if !self.buffered && (self.in_len != 0 || !n.is_multiple_of(FRAME)) {
      // This host's blocks don't line up with frames: 10 ms of latency from now on (once).
      self.buffered = true;
      self.out_queue.extend(std::iter::repeat_n(0.0, FRAME));
    }
    for ((&m, &l), &r) in mic_block.iter().zip(ref_left).zip(ref_right) {
      let at = self.in_len;
      if let (Some(ls), Some(rs), Some(ms)) =
        (self.in_left.get_mut(at), self.in_right.get_mut(at), self.in_mic.get_mut(at))
      {
        // NaN/inf from a driver must not reach the filter state.
        (*ls, *rs, *ms) = (finite(l), finite(r), finite(m));
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

  fn process_frame(&mut self) {
    self.mix_reference();
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

    let [out_left, out_right] = &mut self.render_out;
    let rendered = if self.stereo {
      self.apm.process_render_f32(&[&self.in_left, &self.in_right], &mut [out_left, out_right])
    } else {
      self.apm.process_render_f32(&[&self.in_ref], &mut [out_left])
    };
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

  /// Mix this frame's reference to mono and ask for a stereo AEC3 while its channels differ.
  fn mix_reference(&mut self) {
    // `side`: 4 x the energy of (L - R) / 2. `power`: 2 x the channels' mean energy.
    let (mut side, mut power) = (0.0f32, 0.0f32);
    for ((mix, &l), &r) in self.in_ref.iter_mut().zip(&self.in_left).zip(&self.in_right) {
      *mix = (l + r) * 0.5;
      side += (l - r) * (l - r);
      power += l * l + r * r;
    }
    if power > 2.0 * FRAME as f32 * STEREO_MIN_POWER {
      (self.stereo_run, self.mono_run) = if side * 0.5 > STEREO_SIDE_RATIO * power {
        (self.stereo_run.saturating_add(1), 0)
      } else {
        (0, self.mono_run.saturating_add(1))
      };
    }
    self.wanted_stereo =
      if self.wanted_stereo { self.mono_run < STEREO_OFF_FRAMES } else { self.stereo_run >= STEREO_ON_FRAMES };
    #[cfg(test)]
    if self.variant.split_whitespace().any(|token| token == "mono") {
      self.wanted_stereo = false;
    }
  }

  /// Rebuild AEC3 for the wanted mic gain, fallback echo estimate and reference channels once
  /// it is due. `peak`: this frame's mic peak.
  fn follow_wanted_gain(&mut self, peak: f32) {
    let last_peak = std::mem::replace(&mut self.last_peak, peak);
    if self.wanted_gain_db == self.mic_gain_db
      && self.wanted_fallback_db == self.fallback_db
      && self.wanted_stereo == self.stereo
    {
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
        "[AEC3] mic gain {} -> {} dB, fallback echo {} -> {} dB, stereo reference {} -> {}",
        self.mic_gain_db,
        self.wanted_gain_db,
        self.fallback_db,
        self.wanted_fallback_db,
        self.stereo,
        self.wanted_stereo
      );
      self.rebuild();
    }
  }

  /// New AEC3 for the wanted mic gain, fallback echo estimate and reference channels. Allocates
  /// on the audio thread like `reset`: at a stream's start, and when the learned speech level or
  /// the measured echo strength moves by a step or the reference turns stereo or mono (once per
  /// 2 s at most).
  fn rebuild(&mut self) {
    self.mic_gain_db = self.wanted_gain_db;
    self.mic_gain = 10f32.powf(self.mic_gain_db / 20.0);
    self.fallback_db = self.wanted_fallback_db;
    self.stereo = self.wanted_stereo;
    #[allow(unused_mut)] // only the tests change it
    let mut config = tuning(self.mic_gain, 10f32.powf(self.fallback_db / 20.0), self.stereo);
    #[cfg(test)]
    tests::apply_variant(&mut config, &self.variant, self.mic_gain);
    self.apm = build_apm(config, self.stereo);
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

fn build_apm(tuning: EchoCanceller3Config, stereo: bool) -> AudioProcessing {
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
    .render_config(if stereo { StreamConfig::new(48_000, 2) } else { stream })
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
///
/// `stereo`: AEC3 gets both reference channels. For its fallback it adds their powers up, where
/// the echo strength is measured against their mix: with the same sound on both that is half the
/// sum (so the gain goes down 3 dB), otherwise less (the estimate is then over, by 3 dB with
/// unrelated channels, and never under). `mix_reference` decides when, and remembers it over a rebuild: AEC3's own stereo
/// detector is set to follow at the first frame and never to go back. It cannot be switched
/// off: sonora 0.3.0 then stays on the left channel alone (its constructor starts mono and only
/// the detector ever changes that; 2.5 dB instead of 35 dB removed with independent channels).
/// AEC3's own multichannel settings (`create_default_multichannel_config`: faster coarse
/// filter, slower suppressor gain) are left out: on synthetic stereo music they removed no more
/// echo (43.6 dB either way) and took 10 dB more off a voice at the echo's level.
fn tuning(mic_gain: f32, fallback_gain: f32, stereo: bool) -> EchoCanceller3Config {
  let mut cfg = EchoCanceller3Config::default();
  cfg.ep_strength.default_gain = fallback_gain * mic_gain * if stereo { std::f32::consts::FRAC_1_SQRT_2 } else { 1.0 };
  if stereo {
    cfg.multi_channel.stereo_detection_hysteresis_seconds = 0.0;
    cfg.multi_channel.stereo_detection_timeout_threshold_seconds = 0;
  }
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
  if x.is_finite() { x } else { 0.0 }
}

#[cfg(test)]
mod tests;
