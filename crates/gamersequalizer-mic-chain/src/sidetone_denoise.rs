//! Frame-based noise suppressors for the sidetone (`gamersequalizer_common::sidetone`):
//! WebRTC NS, RNNoise (2017) and RNNoise 0.2. They work on 10 ms frames at 48 kHz, so unlike
//! the band expander they add delay; the user picks one knowing that.
//!
//! Delay on blocks of whole frames (measured, `delay_matches_the_measured_values`): WebRTC NS
//! 333 samples, RNNoise 480, RNNoise 0.2 960. Blocks of any other size go through a queue,
//! which adds one more frame (10 ms).
//!
//! Limits:
//! - 48 kHz only; the caller runs the band expander at any other rate.
//! - Switching the mode starts from silence: a gap as long as the new mode's delay.
//! - WebRTC NS runs at a fixed level (`High`); the depth applies to the two RNNoise modes.
//! - RNNoise (2017) started on nothing but steady hiss takes only 3-9 dB off it (white noise
//!   at -50 / -40 dBFS; at -60 dBFS, or once it has heard a voice, it takes all of it).
//!   WebRTC NS and RNNoise 0.2 do not need the voice first.

use gamersequalizer_common::sidetone::NoiseMode;
use nnnoiseless::DenoiseState;
use rnnoise2::DenoiseState as DenoiseState2;
use sonora::config::{MaxProcessingRate, NoiseSuppression, NoiseSuppressionLevel, Pipeline};
use sonora::{AudioProcessing, Config, StreamConfig};

/// Samples per frame of all three suppressors (10 ms).
pub const FRAME: usize = 480;
/// The only sample rate the suppressors run at.
pub const SAMPLE_RATE: u32 = 48_000;
/// RNNoise is trained on i16-range samples, not [-1.0, 1.0].
const I16_SCALE: f32 = 32768.0;
/// Delay of the WebRTC suppressor inside `sonora` (band split + overlap), measured.
const WEBRTC_NS_DELAY: usize = 333;
/// Silent frames that push old audio out of a suppressor when its mode starts (RNNoise's
/// pitch buffer holds 1728 samples, the WebRTC one a third of a frame).
const FLUSH_FRAMES: usize = 4;
const QUEUE: usize = 2 * FRAME;
const MAX_DEPTH_DB: f32 = 40.0;

/// All three suppressors, built once (not on the audio thread), one running at a time.
pub struct SidetoneDenoiser {
  ns: Box<AudioProcessing>,
  rnnoise: Box<DenoiseState<'static>>,
  /// None: the weights did not load; RNNoise (2017) stands in.
  rnnoise2: Option<Box<DenoiseState2>>,
  /// Mode the state below belongs to; None = nothing ran since the last pause.
  running: Option<NoiseMode>,
  /// Blocks have been whole frames so far: processed in place, without the queue.
  direct: bool,
  pending: Vec<f32>,
  pending_len: usize,
  ready: Vec<f32>,
  ready_start: usize,
  ready_len: usize,
  /// The last two input frames: the dry signal, delayed like the network's output.
  dry: [Vec<f32>; 2],
  scratch_in: Vec<f32>,
  scratch_out: Vec<f32>,
}

impl Default for SidetoneDenoiser {
  fn default() -> Self {
    Self::new()
  }
}

impl SidetoneDenoiser {
  pub fn new() -> Self {
    let stream = StreamConfig::new(SAMPLE_RATE, 1);
    let config = Config {
      pipeline: Pipeline { maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz, ..Default::default() },
      noise_suppression: Some(NoiseSuppression {
        level: NoiseSuppressionLevel::High,
        analyze_linear_aec_output_when_available: false,
      }),
      ..Default::default()
    };
    Self {
      ns: Box::new(AudioProcessing::builder().config(config).capture_config(stream).render_config(stream).build()),
      rnnoise: DenoiseState::new(),
      rnnoise2: DenoiseState2::with_model(rnnoise2::Model::Full).ok(),
      running: None,
      direct: true,
      pending: vec![0.0; FRAME],
      pending_len: 0,
      ready: vec![0.0; QUEUE],
      ready_start: 0,
      ready_len: 0,
      dry: [vec![0.0; FRAME], vec![0.0; FRAME]],
      scratch_in: vec![0.0; FRAME],
      scratch_out: vec![0.0; FRAME],
    }
  }

  /// The mode that runs when `mode` is asked for (RNNoise 0.2 without weights = RNNoise).
  pub fn available(&self, mode: NoiseMode) -> NoiseMode {
    if mode == NoiseMode::RnNoise2 && self.rnnoise2.is_none() { NoiseMode::RnNoise } else { mode }
  }

  /// Delay `process` adds right now in `mode`, in samples at 48 kHz (0 for the other modes).
  pub fn delay_samples(&self, mode: NoiseMode) -> usize {
    let mode = self.available(mode);
    if !mode.is_framed() {
      return 0;
    }
    own_delay_samples(mode) + if self.direct { 0 } else { FRAME }
  }

  /// In place on 48 kHz mono `samples`. `depth_db`: how far noise is turned down at most
  /// (RNNoise modes). Returns the mode that ran; for a mode that is not frame-based it
  /// leaves the samples alone.
  pub fn process(&mut self, mode: NoiseMode, depth_db: f32, samples: &mut [f32]) -> NoiseMode {
    let mode = self.available(mode);
    if !mode.is_framed() {
      self.running = None;
      return mode;
    }
    if self.running != Some(mode) {
      self.start(mode);
    }
    let depth_db = if depth_db.is_finite() { depth_db.clamp(0.0, MAX_DEPTH_DB) } else { 0.0 };
    let dry_share = 10f32.powf(-depth_db / 20.0);

    if self.direct && samples.len().is_multiple_of(FRAME) {
      for frame in samples.as_chunks_mut::<FRAME>().0 {
        self.run_frame(mode, dry_share, frame);
      }
      return mode;
    }
    if self.direct {
      // From here on through the queue, with one frame of silence ahead so it never runs dry.
      self.direct = false;
      self.ready.fill(0.0);
      self.ready_start = 0;
      self.ready_len = FRAME;
    }
    for sample in samples.iter_mut() {
      if let Some(slot) = self.pending.get_mut(self.pending_len) {
        *slot = *sample;
        self.pending_len += 1;
      }
      if self.pending_len >= FRAME {
        let mut frame = std::mem::take(&mut self.pending);
        self.run_frame(mode, dry_share, &mut frame);
        self.push_ready(&frame);
        self.pending = frame;
        self.pending_len = 0;
      }
      *sample = self.pop_ready();
    }
    mode
  }

  fn start(&mut self, mode: NoiseMode) {
    self.running = Some(mode);
    self.direct = true;
    self.pending_len = 0;
    self.ready_len = 0;
    if let Some(d0) = self.dry.first_mut() {
      d0.fill(0.0);
    }
    if let Some(d1) = self.dry.get_mut(1) {
      d1.fill(0.0);
    }
    self.scratch_in.fill(0.0);
    for _ in 0..FLUSH_FRAMES {
      match mode {
        NoiseMode::RnNoise => {
          self.rnnoise.process_frame(&mut self.scratch_out, &self.scratch_in);
        }
        NoiseMode::RnNoise2 => {
          if let Some(net) = self.rnnoise2.as_mut() {
            net.process_frame(&mut self.scratch_out, &self.scratch_in);
          }
        }
        // The WebRTC suppressor holds a third of a frame of audio from its last run.
        NoiseMode::WebRtcNs => {
          let _ = self.ns.process_capture_f32(&[&self.scratch_in], &mut [&mut self.scratch_out]);
        }
        NoiseMode::Off | NoiseMode::Expander => {}
      }
    }
  }

  /// One frame through the running suppressor, in place.
  fn run_frame(&mut self, mode: NoiseMode, dry_share: f32, frame: &mut [f32]) {
    if let (Some(dst), Some(src)) = (self.scratch_in.get_mut(..FRAME), frame.get(..FRAME)) {
      dst.copy_from_slice(src);
    }
    // The dry signal each mode mixes back in: delayed like its output.
    let dry_idx = match mode {
      NoiseMode::RnNoise2 => 1,
      _ => 0,
    };
    let ran = match mode {
      NoiseMode::WebRtcNs => self.ns.process_capture_f32(&[&self.scratch_in], &mut [&mut self.scratch_out]).is_ok(),
      NoiseMode::RnNoise | NoiseMode::RnNoise2 => {
        self.scratch_in.iter_mut().for_each(|s| *s *= I16_SCALE);
        match (mode, self.rnnoise2.as_mut()) {
          (NoiseMode::RnNoise2, Some(net)) => {
            net.process_frame(&mut self.scratch_out, &self.scratch_in);
          }
          _ => {
            self.rnnoise.process_frame(&mut self.scratch_out, &self.scratch_in);
          }
        }
        let dry = self.dry.get(dry_idx).map(Vec::as_slice).unwrap_or(&[]);
        for (out, dry) in self.scratch_out.iter_mut().zip(dry.iter()) {
          *out = *out / I16_SCALE * (1.0 - dry_share) + dry * dry_share;
        }
        true
      }
      NoiseMode::Off | NoiseMode::Expander => false,
    };
    // The older frame moves to [1] without a copy (audio thread: no allocation).
    self.dry.swap(0, 1);
    if let (Some(d0), Some(src)) = (self.dry.first_mut(), frame.get(..FRAME)) {
      d0.copy_from_slice(src);
    }
    if ran && let Some(dst) = frame.get_mut(..FRAME) {
      for (dst, src) in dst.iter_mut().zip(self.scratch_out.iter()) {
        *dst = if src.is_finite() { *src } else { 0.0 };
      }
    }
  }

  fn push_ready(&mut self, frame: &[f32]) {
    for sample in frame {
      let index = (self.ready_start + self.ready_len) % QUEUE;
      if let Some(slot) = self.ready.get_mut(index) {
        *slot = *sample;
      }
      if self.ready_len < QUEUE {
        self.ready_len += 1;
      } else {
        // Full (cannot happen: one sample leaves for each that enters): drop the oldest.
        self.ready_start = (self.ready_start + 1) % QUEUE;
      }
    }
  }

  fn pop_ready(&mut self) -> f32 {
    if self.ready_len == 0 {
      return 0.0;
    }
    let sample = self.ready.get(self.ready_start).copied().unwrap_or(0.0);
    self.ready_start = (self.ready_start + 1) % QUEUE;
    self.ready_len -= 1;
    sample
  }
}

/// Delay `mode` adds on blocks of whole frames, ms: what the window shows before it runs.
pub fn nominal_delay_ms(mode: NoiseMode) -> f32 {
  own_delay_samples(mode) as f32 * 1000.0 / SAMPLE_RATE as f32
}

/// The suppressor's own delay in samples at 48 kHz, without the queue (0 for the other modes).
fn own_delay_samples(mode: NoiseMode) -> usize {
  match mode {
    NoiseMode::WebRtcNs => WEBRTC_NS_DELAY,
    NoiseMode::RnNoise => FRAME,
    NoiseMode::RnNoise2 => 2 * FRAME,
    NoiseMode::Off | NoiseMode::Expander => 0,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::f32::consts::PI;

  const MODES: [NoiseMode; 3] = [NoiseMode::WebRtcNs, NoiseMode::RnNoise, NoiseMode::RnNoise2];

  #[test]
  fn delay_matches_the_measured_values() {
    let mut denoiser = SidetoneDenoiser::new();
    for mode in MODES {
      for block in [FRAME, 128] {
        let input = speech(6 * SAMPLE_RATE as usize);
        let mut out = input.clone();
        // A pause between runs, so every run starts fresh (as after a mode switch).
        denoiser.process(NoiseMode::Expander, 0.0, &mut []);
        for chunk in out.chunks_mut(block) {
          assert_eq!(denoiser.process(mode, 18.0, chunk), mode);
        }
        assert!(out.iter().all(|s| s.is_finite()));
        let measured = delay(&input, &out);
        let reported = denoiser.delay_samples(mode);
        assert_eq!(measured, reported, "{mode:?}, blocks of {block}");
        let expected = match mode {
          NoiseMode::WebRtcNs => 333,
          NoiseMode::RnNoise => 480,
          _ => 960,
        } + if block == FRAME { 0 } else { FRAME };
        assert_eq!(reported, expected, "{mode:?}, blocks of {block}");
      }
    }
  }

  #[test]
  fn steady_hiss_is_turned_down() {
    let mut denoiser = SidetoneDenoiser::new();
    for (mode, at_least_db) in [(NoiseMode::WebRtcNs, 12.0), (NoiseMode::RnNoise, 15.0), (NoiseMode::RnNoise2, 15.0)] {
      // A voice first: RNNoise (2017) barely touches hiss it has heard nothing else with.
      let mut warm_up = speech(4 * SAMPLE_RATE as usize);
      let input = hiss(8 * SAMPLE_RATE as usize);
      let mut out = input.clone();
      denoiser.process(NoiseMode::Off, 0.0, &mut []);
      for chunk in warm_up.chunks_mut(FRAME).chain(out.chunks_mut(FRAME)) {
        denoiser.process(mode, 18.0, chunk);
      }
      let tail = 6 * SAMPLE_RATE as usize;
      let cut = 20.0 * (rms(&out[tail..]) / rms(&input[tail..])).log10();
      assert!(cut < -at_least_db, "{mode:?}: hiss changed by {cut:.1} dB");
      // The depth is the limit for the RNNoise modes.
      if mode != NoiseMode::WebRtcNs {
        assert!(cut > -19.0, "{mode:?}: deeper than the 18 dB asked for ({cut:.1} dB)");
      }
    }
  }

  #[test]
  fn nothing_from_before_a_pause_is_played() {
    let mut denoiser = SidetoneDenoiser::new();
    for mode in MODES {
      // A loud voice, a pause (muted, or another mode), then silence in: silence out.
      let mut before = speech(SAMPLE_RATE as usize);
      before.iter_mut().for_each(|s| *s *= 5.0);
      before.chunks_mut(FRAME).for_each(|chunk| {
        denoiser.process(mode, 18.0, chunk);
      });
      denoiser.process(NoiseMode::Off, 0.0, &mut []);
      let mut after = vec![0.0f32; 4 * FRAME];
      after.chunks_mut(FRAME).for_each(|chunk| {
        denoiser.process(mode, 18.0, chunk);
      });
      let peak = after.iter().fold(0.0f32, |p, s| p.max(s.abs()));
      assert!(peak < 1e-3, "{mode:?}: {peak} left from before the pause");
    }
  }

  #[test]
  fn other_modes_leave_the_samples_alone() {
    let mut denoiser = SidetoneDenoiser::new();
    let input = speech(960);
    for mode in [NoiseMode::Off, NoiseMode::Expander] {
      let mut out = input.clone();
      assert_eq!(denoiser.process(mode, 18.0, &mut out), mode);
      assert_eq!(out, input);
      assert_eq!(denoiser.delay_samples(mode), 0);
    }
  }

  #[test]
  fn garbage_in_gives_finite_out() {
    let mut denoiser = SidetoneDenoiser::new();
    for mode in MODES {
      let mut block = speech(4 * FRAME);
      block[100] = f32::NAN;
      block[700] = f32::INFINITY;
      denoiser.process(mode, f32::NAN, &mut block);
      assert!(block.iter().all(|s| s.is_finite()), "{mode:?}");
    }
  }

  /// Speech-like: 120 Hz harmonics with a 4 Hz syllable envelope, over hiss at -50 dBFS.
  fn speech(len: usize) -> Vec<f32> {
    let noise = hiss(len);
    (0..len)
      .map(|i| {
        let t = i as f32 / SAMPLE_RATE as f32;
        let envelope = (0.5 - 0.5 * (2.0 * PI * 4.0 * t).cos()).powi(2);
        let f0 = 120.0 + 20.0 * (2.0 * PI * 0.7 * t).sin();
        let voice: f32 = (1..30).map(|h| (2.0 * PI * f0 * h as f32 * t).sin() / h as f32).sum();
        0.1 * envelope * voice + noise[i]
      })
      .collect()
  }

  /// White noise at about -50 dBFS.
  fn hiss(len: usize) -> Vec<f32> {
    let mut state = 7u32;
    (0..len)
      .map(|_| {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        0.0055 * ((state >> 8) as f32 / 8_388_608.0 - 1.0)
      })
      .collect()
  }

  /// Lag of the cross-correlation peak over the second half.
  fn delay(input: &[f32], output: &[f32]) -> usize {
    let from = input.len() / 2;
    let score = |lag: usize| (from..input.len()).map(|i| f64::from(input[i - lag]) * f64::from(output[i])).sum::<f64>();
    (0..2_400).max_by(|a, b| score(*a).total_cmp(&score(*b))).unwrap()
  }

  fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / samples.len().max(1) as f64).sqrt() as f32
  }
}
