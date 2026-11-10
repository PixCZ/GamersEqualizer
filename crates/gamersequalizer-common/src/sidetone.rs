//! Sidetone: the user's own voice in their headphones (sealed in-ears block it). This file
//! holds the parts that do not depend on Windows:
//!
//! mic APO: raw input -> `SidetoneChain` (cuts, band expander, level) -> `SidetoneRing` (in
//! the Global mapping) -> output APO: `SidetoneReader` -> mixed in before the output limiter.
//!
//! Nothing here adds delay: IIR filters and per-sample gains only, no lookahead. The delay of
//! the whole path is the audio engine's periods plus what `SidetoneReader` keeps queued so
//! that it never runs dry.
//!
//! `SidetonePairs` is the saved "microphone + output" matrix; the daemon owns its file.
//!
//! Limits:
//! - `SidetoneReader` has no anti-alias filter: a mic running at a higher rate than the
//!   output folds what is above the output's Nyquist back (a voice has little up there).
//! - A reader that runs dry cuts to silence at once (a click); it then keeps a larger margin.
//! - The expander's bands overlap gently (differences of 12 dB/octave low-passes): a closed
//!   band takes up to ~1 dB from a tone in the band next to it.

use crate::biquad::BiquadCascade;
use crate::profile::{BiquadBand, FilterType};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Samples in the ring: 341 ms at 48 kHz, 85 ms at 192 kHz.
pub const SIDETONE_RING_LEN: usize = 16_384;
const RING_MASK: u32 = SIDETONE_RING_LEN as u32 - 1;
const _: () = assert!(SIDETONE_RING_LEN.is_power_of_two());
/// A sample beyond this is garbage (a torn write, a stale mapping): read as silence.
const SAMPLE_MAX: f32 = 4.0;
const HNS_PER_SEC: f64 = 10_000_000.0;

pub const GAIN_DB_MIN: f32 = -40.0;
pub const GAIN_DB_MAX: f32 = 12.0;
pub const LOW_CUT_HZ_MIN: f32 = 80.0;
pub const LOW_CUT_HZ_MAX: f32 = 400.0;
pub const HIGH_CUT_HZ_MIN: f32 = 4_000.0;
pub const HIGH_CUT_HZ_MAX: f32 = 12_000.0;
pub const GATE_MARGIN_DB_MIN: f32 = 3.0;
pub const GATE_MARGIN_DB_MAX: f32 = 20.0;
pub const GATE_RANGE_DB_MAX: f32 = 40.0;
/// The Windows mic volume is taken back out of the sidetone by at most this much, so a mic
/// at 10 % does not get its noise boosted by 40 dB.
const MIC_VOLUME_COMPENSATION_MAX_DB: f32 = 20.0;
/// The level moves to a new value over about this long (no zipper noise, no click on mute).
const LEVEL_RAMP_SECONDS: f32 = 0.010;

/// Band edges of the expander: below 500 Hz, 500 Hz - 2 kHz, 2 - 5 kHz, above 5 kHz.
const BAND_EDGES_HZ: [f32; 3] = [500.0, 2_000.0, 5_000.0];
const BANDS: usize = BAND_EDGES_HZ.len() + 1;
/// Low cut of the one-band detector: rumble must not hold the gate open.
const WIDE_DETECTOR_LOW_CUT_HZ: f32 = 100.0;
/// The level, the noise floor and the gain target are updated once per this many samples.
const CONTROL_SAMPLES: u32 = 16;
/// The expander filters run on chunks of this many samples (scratch buffer size).
const CHUNK: usize = 256;
/// Fast level: peak, falling with this time constant.
const ENV_RELEASE_SECONDS: f32 = 0.010;
/// Slow level (average of the fast one): its minimum over `FLOOR_WINDOWS` windows of
/// `FLOOR_WINDOW_SECONDS` is the band's noise floor.
const ENV_SLOW_SECONDS: f32 = 0.100;
const FLOOR_WINDOW_SECONDS: f32 = 0.5;
const FLOOR_WINDOWS: usize = 6;
/// The floor stays inside -90..-30 dBFS: digital silence must not pull it to nothing, and a
/// long loud sound must not push the threshold above a voice.
const FLOOR_MIN: f32 = 3.16e-5;
const FLOOR_MAX: f32 = 0.0316;
/// A band goes from closed to open over this many dB below "floor + margin".
const KNEE_DB: f32 = 4.0;
const ATTACK_SECONDS: f32 = 0.001;
const HOLD_SECONDS: f32 = 0.150;
const RELEASE_SECONDS: f32 = 0.150;

/// Reader: queued audio kept beyond what one call needs, against late calls of either APO.
const MARGIN_SECONDS: f32 = 0.002;
/// Every time the reader runs dry the margin grows by this much, up to the maximum.
const MARGIN_STEP_SECONDS: f32 = 0.001;
const MARGIN_MAX_SECONDS: f32 = 0.010;
/// Queued on top of the margin at the start, until the first window has shown how much the
/// writer's blocks need (then the reader moves up to the margin).
const START_EXTRA_SECONDS: f32 = 0.010;
/// The lowest queue level over this long steers the reader.
const WINDOW_SECONDS: f32 = 0.25;
/// More spare queue than this is cut at once (a short fade) instead of slowly played off.
const HARD_RESYNC_SECONDS: f32 = 0.010;
/// The first window after a start cuts anything above this.
const FIRST_ALIGN_SECONDS: f32 = 0.001;
/// A spare queue is played off (or built up) over this long, by reading faster (slower).
const SOFT_SECONDS: f64 = 2.0;
/// ...by at most this much: 0.3 % is 5 cents, not audible on a voice.
const MAX_RATE_ADJUST: f64 = 0.003;
const FADE_SECONDS: f32 = 0.002;
/// This much queued beyond one call's need can only be audio from before a pause of the
/// reader: it is dropped, not played late.
const STALE_SECONDS: f32 = 0.050;

/// What suppresses noise in the sidetone. Crosses the shared memory as a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoiseMode {
  Off,
  /// Band expander in this file: no delay.
  #[default]
  Expander,
  /// WebRTC noise suppressor (`sonora`): frames, ~7 ms.
  WebRtcNs,
  /// RNNoise 2017 (`nnnoiseless`): frames, 10 ms.
  RnNoise,
  /// RNNoise 0.2 (`rnnoise2`): frames, 20 ms.
  RnNoise2,
}

/// What the user set for one microphone + output pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SidetoneSettings {
  pub enabled: bool,
  pub noise_mode: NoiseMode,
  /// Sidetone level relative to the raw microphone, dB.
  pub gain_db: f32,
  /// High-pass corner, Hz; 0 = off.
  pub low_cut_hz: f32,
  /// Low-pass corner, Hz; 0 = off.
  pub high_cut_hz: f32,
  /// A band opens this many dB above its noise floor (sensitivity).
  pub gate_margin_db: f32,
  /// A closed band is turned down by this many dB (depth).
  pub gate_range_db: f32,
}

/// One saved pair. Unknown JSON fields are ignored, missing ones take the defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidetonePair {
  pub mic_id: String,
  pub output_id: String,
  /// Names as shown when the pair was saved (for the list; the ids decide).
  pub mic_name: String,
  pub output_name: String,
  pub enabled: bool,
  /// `NoiseMode::to_u32`; a mode from a newer version stays as its number.
  pub noise_mode: u32,
  pub gain_db: f32,
  pub low_cut_hz: f32,
  pub high_cut_hz: f32,
  pub gate_margin_db: f32,
  pub gate_range_db: f32,
}

/// The saved matrix (`profiles/sidetone.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidetonePairs {
  pub pairs: Vec<SidetonePair>,
}

/// Mono sample ring in shared memory: one writer (the mic APO), readers that keep their own
/// position. All zero = empty, which is how a fresh mapping starts.
#[repr(C)]
pub struct SidetoneRing {
  /// Samples written so far (wraps). Stored after the samples.
  write_index: AtomicU32,
  /// Sample rate of the samples, Hz; 0 = nothing written yet.
  rate: AtomicU32,
  /// Capture time (QPC, 100 ns units) of the sample at `write_index`; 0 = unknown.
  write_qpc_hns: AtomicU64,
  /// Writer: (process id << 32) | instance; 0 = none. A value of another process is stale.
  owner: AtomicU64,
  samples: [AtomicU32; SIDETONE_RING_LEN],
}

/// What one `SidetoneReader::read` call did.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReadInfo {
  /// Audio was written to the output (false: silence, nothing or not enough queued).
  pub playing: bool,
  /// Ring samples between the read position and the write head after this call.
  pub queued: u32,
  /// Sample rate of the ring, Hz.
  pub ring_rate: u32,
}

/// Reads the ring at the output's sample rate and keeps as little queued as is safe.
///
/// The writer delivers blocks at its own clock, the reader is called at another. Each call
/// needs `need` ring samples; the reader steers the lowest spare queue seen over a window
/// (`avail - need`) to the margin: by reading up to 0.3 % faster or slower, or with a short
/// fade and a jump when it is far off or right after a start.
#[derive(Debug, Clone)]
pub struct SidetoneReader {
  state: ReaderState,
  ring_rate: u32,
  /// Ring index of the sample the next output sample starts at, plus the fraction.
  base: u32,
  frac: f64,
  /// Relative speed correction.
  adjust: f64,
  fade: f32,
  /// The fade is going down for a jump of this many ring samples.
  pending_jump: u32,
  margin_seconds: f32,
  window_frames: u32,
  /// Lowest `avail - need` in this window.
  min_spare: i64,
  first_window: bool,
  underruns: u32,
  resyncs: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderState {
  /// Nothing to read: the next call starts queueing at the write head.
  Idle,
  /// Waiting until enough is queued.
  Priming,
  Running,
}

/// Downward expander in `BANDS` bands, or in one. Each band measures its own noise floor in
/// the pauses and is turned down whenever nothing louder is in it.
///
/// The bands are differences of low-passes (band k = LP k - LP k-1, the last one input - LP),
/// so with all gains at 1 their sum is the input again. The levels come from separate, steep
/// band-passes: a loud vowel must not open the hiss band.
#[derive(Debug, Clone)]
pub struct BandExpander {
  split: [BiquadCascade; BANDS - 1],
  detect: [BiquadCascade; BANDS],
  wide: BiquadCascade,
  bands: [BandState; BANDS],
  /// One gain for all bands, from the wide detector (a plain expander).
  single_band: bool,
  phase: u32,
  env_release: f32,
  env_slow: f32,
  attack: f32,
  release: f32,
  hold_samples: u32,
  window_ticks: u32,
  margin: f32,
  range_db: f32,
  split_buf: [[f32; CHUNK]; BANDS - 1],
  detect_buf: [[f32; CHUNK]; BANDS],
  wide_buf: [f32; CHUNK],
}

#[derive(Debug, Clone, Copy)]
struct BandState {
  /// Peak of the detector since the last control tick.
  peak: f32,
  env: f32,
  env_slow: f32,
  /// Minimum of `env_slow` in the running window and in the windows before it.
  window_min: f32,
  mins: [f32; FLOOR_WINDOWS],
  next_min: usize,
  ticks: u32,
  target: f32,
  gain: f32,
  hold: u32,
}

/// The sidetone processing in the mic APO, in place on the raw input (first channel):
/// `run_cuts` -> noise suppression -> `run_level`. `process` does all three with the band
/// expander; the frame-based modes run their own suppressor between `run_cuts` and
/// `run_level`.
#[derive(Debug, Clone)]
pub struct SidetoneChain {
  rate: f32,
  cuts: BiquadCascade,
  cut_hz: (f32, f32),
  expander: BandExpander,
  noise_mode: NoiseMode,
  level: f32,
  level_goal: f32,
  level_step: f32,
}

impl NoiseMode {
  pub fn to_u32(self) -> u32 {
    match self {
      NoiseMode::Off => 0,
      NoiseMode::Expander => 1,
      NoiseMode::WebRtcNs => 2,
      NoiseMode::RnNoise => 3,
      NoiseMode::RnNoise2 => 4,
    }
  }

  /// A mode this build does not know (settings from a newer one) runs as the expander.
  pub fn from_u32(v: u32) -> Self {
    match v {
      0 => NoiseMode::Off,
      2 => NoiseMode::WebRtcNs,
      3 => NoiseMode::RnNoise,
      4 => NoiseMode::RnNoise2,
      _ => NoiseMode::Expander,
    }
  }

  /// Works on 10 ms frames at 48 kHz, i.e. adds delay.
  pub fn is_framed(self) -> bool {
    matches!(self, NoiseMode::WebRtcNs | NoiseMode::RnNoise | NoiseMode::RnNoise2)
  }
}

impl Default for SidetoneSettings {
  fn default() -> Self {
    Self {
      enabled: false,
      noise_mode: NoiseMode::Expander,
      gain_db: -20.0,
      low_cut_hz: 200.0,
      high_cut_hz: 0.0,
      gate_margin_db: 10.0,
      gate_range_db: 18.0,
    }
  }
}

impl SidetoneSettings {
  /// Every value inside its range; a value that is not a number becomes the default.
  pub fn clamped(self) -> Self {
    let d = Self::default();
    let or = |v: f32, default: f32| if v.is_finite() { v } else { default };
    let cut = |v: f32, default: f32, min: f32, max: f32| {
      let v = or(v, default);
      if v < 1.0 {
        0.0
      } else {
        v.clamp(min, max)
      }
    };
    Self {
      enabled: self.enabled,
      noise_mode: self.noise_mode,
      gain_db: or(self.gain_db, d.gain_db).clamp(GAIN_DB_MIN, GAIN_DB_MAX),
      low_cut_hz: cut(self.low_cut_hz, d.low_cut_hz, LOW_CUT_HZ_MIN, LOW_CUT_HZ_MAX),
      high_cut_hz: cut(self.high_cut_hz, d.high_cut_hz, HIGH_CUT_HZ_MIN, HIGH_CUT_HZ_MAX),
      gate_margin_db: or(self.gate_margin_db, d.gate_margin_db).clamp(GATE_MARGIN_DB_MIN, GATE_MARGIN_DB_MAX),
      gate_range_db: or(self.gate_range_db, d.gate_range_db).clamp(0.0, GATE_RANGE_DB_MAX),
    }
  }
}

impl Default for SidetonePair {
  fn default() -> Self {
    Self::new("", "", "", "")
  }
}

impl SidetonePair {
  /// A new pair with the default settings, switched off.
  pub fn new(mic_id: &str, output_id: &str, mic_name: &str, output_name: &str) -> Self {
    let s = SidetoneSettings::default();
    Self {
      mic_id: mic_id.to_string(),
      output_id: output_id.to_string(),
      mic_name: mic_name.to_string(),
      output_name: output_name.to_string(),
      enabled: s.enabled,
      noise_mode: s.noise_mode.to_u32(),
      gain_db: s.gain_db,
      low_cut_hz: s.low_cut_hz,
      high_cut_hz: s.high_cut_hz,
      gate_margin_db: s.gate_margin_db,
      gate_range_db: s.gate_range_db,
    }
  }

  pub fn settings(&self) -> SidetoneSettings {
    SidetoneSettings {
      enabled: self.enabled,
      noise_mode: NoiseMode::from_u32(self.noise_mode),
      gain_db: self.gain_db,
      low_cut_hz: self.low_cut_hz,
      high_cut_hz: self.high_cut_hz,
      gate_margin_db: self.gate_margin_db,
      gate_range_db: self.gate_range_db,
    }
    .clamped()
  }

  /// Takes the settings over; the ids and names stay.
  pub fn set_settings(&mut self, s: &SidetoneSettings) {
    let s = s.clamped();
    self.enabled = s.enabled;
    self.noise_mode = s.noise_mode.to_u32();
    self.gain_db = s.gain_db;
    self.low_cut_hz = s.low_cut_hz;
    self.high_cut_hz = s.high_cut_hz;
    self.gate_margin_db = s.gate_margin_db;
    self.gate_range_db = s.gate_range_db;
  }

  fn is(&self, mic_id: &str, output_id: &str) -> bool {
    self.mic_id.eq_ignore_ascii_case(mic_id) && self.output_id.eq_ignore_ascii_case(output_id)
  }
}

impl SidetonePairs {
  pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
    serde_json::from_str(text)
  }

  pub fn to_json(&self) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(self)
  }

  pub fn find(&self, mic_id: &str, output_id: &str) -> Option<&SidetonePair> {
    self.pairs.iter().find(|p| p.is(mic_id, output_id))
  }

  /// Replaces the pair with the same ids, or adds it.
  pub fn upsert(&mut self, pair: SidetonePair) {
    match self.pairs.iter_mut().find(|p| p.is(&pair.mic_id, &pair.output_id)) {
      Some(slot) => *slot = pair,
      None => self.pairs.push(pair),
    }
  }

  pub fn remove(&mut self, mic_id: &str, output_id: &str) {
    self.pairs.retain(|p| !p.is(mic_id, output_id));
  }
}

impl Default for SidetoneRing {
  fn default() -> Self {
    Self {
      write_index: AtomicU32::new(0),
      rate: AtomicU32::new(0),
      write_qpc_hns: AtomicU64::new(0),
      owner: AtomicU64::new(0),
      samples: std::array::from_fn(|_| AtomicU32::new(0)),
    }
  }
}

impl SidetoneRing {
  /// Becomes the writer if there is none (or only one left behind by another process).
  /// Returns the owner value for `write` and `release_writer`.
  pub fn claim_writer(&self, pid: u32, instance: u32) -> Option<u64> {
    let owner = (u64::from(pid) << 32) | u64::from(instance.max(1));
    let current = self.owner.load(Ordering::Acquire);
    let free = current == 0 || (current >> 32) as u32 != pid;
    (free && self.owner.compare_exchange(current, owner, Ordering::AcqRel, Ordering::Relaxed).is_ok()).then_some(owner)
  }

  /// Gives the ring up. The rate goes to 0, which tells the readers "stopped on purpose":
  /// they go quiet without counting it as running dry.
  pub fn release_writer(&self, owner: u64) {
    if self.owner.compare_exchange(owner, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
      self.rate.store(0, Ordering::Relaxed);
    }
  }

  /// Appends a block. `end_qpc_hns`: capture time of the sample after the last one, 0 if
  /// unknown. Does nothing unless `owner` holds the ring.
  pub fn write(&self, owner: u64, samples: &[f32], rate: u32, end_qpc_hns: u64) {
    if self.owner.load(Ordering::Acquire) != owner || owner == 0 {
      return;
    }
    let start = self.write_index.load(Ordering::Relaxed);
    for (i, sample) in samples.iter().enumerate() {
      if let Some(slot) = self.samples.get((start.wrapping_add(i as u32) & RING_MASK) as usize) {
        slot.store(sample.to_bits(), Ordering::Relaxed);
      }
    }
    self.rate.store(rate, Ordering::Relaxed);
    self.write_qpc_hns.store(end_qpc_hns, Ordering::Relaxed);
    self.write_index.store(start.wrapping_add(samples.len() as u32), Ordering::Release);
  }

  pub fn write_index(&self) -> u32 {
    self.write_index.load(Ordering::Acquire)
  }

  pub fn rate(&self) -> u32 {
    self.rate.load(Ordering::Relaxed)
  }

  /// Capture time (QPC, 100 ns) of the sample `queued` samples behind the write head, for
  /// "how old is what is being played". None when the writer has no clock.
  pub fn capture_qpc_hns(&self, queued: u32) -> Option<u64> {
    let end = self.write_qpc_hns.load(Ordering::Relaxed);
    let rate = self.rate();
    if end == 0 || rate == 0 {
      return None;
    }
    let behind = (f64::from(queued) * HNS_PER_SEC / f64::from(rate)) as u64;
    Some(end.saturating_sub(behind))
  }

  fn sample(&self, index: u32) -> f32 {
    let value =
      self.samples.get((index & RING_MASK) as usize).map_or(0.0, |s| f32::from_bits(s.load(Ordering::Relaxed)));
    if value.is_finite() && value.abs() <= SAMPLE_MAX {
      value
    } else {
      0.0
    }
  }
}

impl Default for SidetoneReader {
  fn default() -> Self {
    Self {
      state: ReaderState::Idle,
      ring_rate: 0,
      base: 0,
      frac: 0.0,
      adjust: 0.0,
      fade: 0.0,
      pending_jump: 0,
      margin_seconds: MARGIN_SECONDS,
      window_frames: 0,
      min_spare: i64::MAX,
      first_window: true,
      underruns: 0,
      resyncs: 0,
    }
  }
}

impl SidetoneReader {
  /// Times the reader ran dry while playing (each one is an audible gap).
  pub fn underruns(&self) -> u32 {
    self.underruns
  }

  /// Jumps taken because far too much was queued (the first alignment after a start is not
  /// counted).
  pub fn resyncs(&self) -> u32 {
    self.resyncs
  }

  /// The caller stops reading for now (sidetone off): the next `read` starts at the write
  /// head. The margin and the counters stay.
  pub fn pause(&mut self) {
    self.state = ReaderState::Idle;
  }

  /// Fills `out` (mono, `out_rate` Hz) with the next sidetone samples, or with silence.
  pub fn read(&mut self, ring: &SidetoneRing, out: &mut [f32], out_rate: f32) -> ReadInfo {
    out.fill(0.0);
    let rate = ring.rate();
    let head = ring.write_index();
    let silent = |queued: u32| ReadInfo { playing: false, queued, ring_rate: rate };
    if rate == 0 || out.is_empty() || !out_rate.is_finite() || out_rate < 8_000.0 {
      self.state = ReaderState::Idle;
      return silent(0);
    }
    if rate != self.ring_rate {
      self.ring_rate = rate;
      self.state = ReaderState::Idle;
    }
    let ring_rate = rate as f32;
    let step = f64::from(rate) / f64::from(out_rate) * (1.0 + self.adjust);
    // The interpolator reads one sample before and two after the position.
    let need = (self.frac + step * out.len() as f64) as i64 + 3;
    let margin = i64::from((self.margin_seconds * ring_rate) as u32);

    if self.state == ReaderState::Idle {
      // Start at the write head: nothing older (maybe from another session) is played.
      self.base = head;
      self.frac = 0.0;
      self.adjust = 0.0;
      self.fade = 0.0;
      self.pending_jump = 0;
      self.first_window = true;
      self.reset_window();
      self.state = ReaderState::Priming;
    }
    let mut avail = i64::from(head.wrapping_sub(self.base));
    let stale = (need + margin + i64::from((STALE_SECONDS * ring_rate) as u32)).min((SIDETONE_RING_LEN / 2) as i64);
    if avail > stale {
      // The reader was not called for a while (the output stood still): what piled up is old,
      // start over at the write head.
      self.state = ReaderState::Idle;
      return silent(0);
    }
    if self.state == ReaderState::Priming {
      if avail < need + margin + i64::from((START_EXTRA_SECONDS * ring_rate) as u32) {
        return silent(avail as u32);
      }
      self.state = ReaderState::Running;
    }
    if self.pending_jump > 0 && self.fade <= 0.0 {
      // Faded out: skip the spare queue, never into what this call still needs.
      let jump = i64::from(self.pending_jump).min(avail - need - margin).max(0);
      self.base = self.base.wrapping_add(jump as u32);
      avail -= jump;
      self.pending_jump = 0;
    }
    if avail < need {
      self.underruns = self.underruns.wrapping_add(1);
      self.margin_seconds = (self.margin_seconds + MARGIN_STEP_SECONDS).min(MARGIN_MAX_SECONDS);
      self.state = ReaderState::Idle;
      return silent(avail.max(0) as u32);
    }
    self.min_spare = self.min_spare.min(avail - need);

    let fade_step = 1.0 / (FADE_SECONDS * out_rate).max(1.0);
    for o in out.iter_mut() {
      self.fade =
        if self.pending_jump > 0 { (self.fade - fade_step).max(0.0) } else { (self.fade + fade_step).min(1.0) };
      *o = self.interpolate(ring) * self.fade;
      self.frac += step;
      let whole = self.frac as u32;
      self.base = self.base.wrapping_add(whole);
      self.frac -= f64::from(whole);
    }

    self.window_frames += out.len() as u32;
    if self.window_frames as f32 >= WINDOW_SECONDS * out_rate {
      let spare = (self.min_spare - margin) as f32;
      let limit = if self.first_window { FIRST_ALIGN_SECONDS } else { HARD_RESYNC_SECONDS } * ring_rate;
      if spare > limit {
        self.pending_jump = spare as u32;
        self.adjust = 0.0;
        if !self.first_window {
          self.resyncs = self.resyncs.wrapping_add(1);
        }
      } else {
        self.adjust = (f64::from(spare) / (f64::from(rate) * SOFT_SECONDS)).clamp(-MAX_RATE_ADJUST, MAX_RATE_ADJUST);
      }
      self.first_window = false;
      self.reset_window();
    }
    ReadInfo { playing: true, queued: head.wrapping_sub(self.base), ring_rate: rate }
  }

  fn reset_window(&mut self) {
    self.window_frames = 0;
    self.min_spare = i64::MAX;
  }

  /// 4-point, 3rd-order Hermite between `base` and `base + 1` (exact at fraction 0).
  fn interpolate(&self, ring: &SidetoneRing) -> f32 {
    let xm1 = ring.sample(self.base.wrapping_sub(1));
    let x0 = ring.sample(self.base);
    let x1 = ring.sample(self.base.wrapping_add(1));
    let x2 = ring.sample(self.base.wrapping_add(2));
    let t = self.frac as f32;
    let c1 = 0.5 * (x1 - xm1);
    let c2 = xm1 - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
    let c3 = 0.5 * (x2 - xm1) + 1.5 * (x0 - x1);
    ((c3 * t + c2) * t + c1) * t + x0
  }
}

impl BandState {
  fn new() -> Self {
    Self {
      peak: 0.0,
      env: 0.0,
      env_slow: 0.0,
      window_min: f32::MAX,
      // Nothing measured yet: the floor is the level itself, i.e. every band starts closed.
      mins: [f32::MAX; FLOOR_WINDOWS],
      next_min: 0,
      ticks: 0,
      target: 0.0,
      gain: 0.0,
      hold: 0,
    }
  }
}

impl BandExpander {
  pub fn new(sample_rate: f32) -> Self {
    let cut = |filter_type: FilterType, frequency: f32, slope_db: u8| BiquadBand {
      enabled: true,
      filter_type,
      frequency,
      gain_db: 0.0,
      q: std::f32::consts::FRAC_1_SQRT_2,
      slope_db,
    };
    let cascade = |bands: &[BiquadBand]| {
      let mut c = BiquadCascade::default();
      c.set_bands(bands, sample_rate);
      c
    };
    let edge = |k: usize| BAND_EDGES_HZ.get(k).copied();
    let rate = if sample_rate.is_finite() && sample_rate >= 8_000.0 { sample_rate } else { 48_000.0 };
    let tick = CONTROL_SAMPLES as f32 / rate;
    let per_sample = |seconds: f32| 1.0 - (-1.0 / (seconds * rate)).exp();
    Self {
      split: std::array::from_fn(|k| cascade(&[cut(FilterType::LowPass, edge(k).unwrap_or(1_000.0), 12)])),
      detect: std::array::from_fn(|k| {
        let low = k.checked_sub(1).and_then(edge).map(|f| cut(FilterType::HighPass, f, 24));
        let high = edge(k).map(|f| cut(FilterType::LowPass, f, 12));
        cascade(&low.into_iter().chain(high).collect::<Vec<_>>())
      }),
      wide: cascade(&[cut(FilterType::HighPass, WIDE_DETECTOR_LOW_CUT_HZ, 12)]),
      bands: [BandState::new(); BANDS],
      single_band: false,
      phase: 0,
      env_release: (-tick / ENV_RELEASE_SECONDS).exp(),
      env_slow: 1.0 - (-tick / ENV_SLOW_SECONDS).exp(),
      attack: per_sample(ATTACK_SECONDS),
      release: per_sample(RELEASE_SECONDS),
      hold_samples: (HOLD_SECONDS * rate) as u32,
      window_ticks: (FLOOR_WINDOW_SECONDS / tick) as u32,
      margin: 10.0,
      range_db: 18.0,
      split_buf: [[0.0; CHUNK]; BANDS - 1],
      detect_buf: [[0.0; CHUNK]; BANDS],
      wide_buf: [0.0; CHUNK],
    }
  }

  /// Sensitivity (a band opens this many dB above its floor) and depth, both in dB.
  pub fn set(&mut self, margin_db: f32, range_db: f32) {
    self.margin = margin_db.clamp(GATE_MARGIN_DB_MIN, GATE_MARGIN_DB_MAX);
    self.range_db = range_db.clamp(0.0, GATE_RANGE_DB_MAX);
  }

  /// One gain for the whole signal instead of one per band (for comparing the two).
  pub fn set_single_band(&mut self, single_band: bool) {
    self.single_band = single_band;
  }

  /// Gain of the most closed band right now, dB (0 = all open).
  pub fn gain_db(&self) -> f32 {
    let min = self.bands.iter().map(|b| b.gain).fold(1.0f32, f32::min);
    20.0 * min.max(1e-4).log10()
  }

  pub fn reset(&mut self) {
    self.split.iter_mut().chain(self.detect.iter_mut()).for_each(BiquadCascade::reset_state);
    self.wide.reset_state();
    self.bands = [BandState::new(); BANDS];
    self.phase = 0;
  }

  pub fn process(&mut self, samples: &mut [f32]) {
    for chunk in samples.chunks_mut(CHUNK) {
      self.process_chunk(chunk);
    }
  }

  fn process_chunk(&mut self, chunk: &mut [f32]) {
    let n = chunk.len();
    let filter = |cascade: &mut BiquadCascade, buf: &mut [f32; CHUNK]| {
      if let Some(buf) = buf.get_mut(..n) {
        buf.copy_from_slice(chunk);
        cascade.process(buf);
      }
    };
    self.split.iter_mut().zip(self.split_buf.iter_mut()).for_each(|(c, b)| filter(c, b));
    if self.single_band {
      filter(&mut self.wide, &mut self.wide_buf);
    } else {
      self.detect.iter_mut().zip(self.detect_buf.iter_mut()).for_each(|(c, b)| filter(c, b));
    }

    let at = |buf: &[f32; CHUNK], i: usize| buf.get(i).copied().unwrap_or(0.0);
    for (i, sample) in chunk.iter_mut().enumerate() {
      let x = *sample;
      self.phase += 1;
      let tick = self.phase >= CONTROL_SAMPLES;
      if tick {
        self.phase = 0;
      }
      let mut below = 0.0;
      let mut y = 0.0;
      for (k, band) in self.bands.iter_mut().enumerate() {
        // Band k = low-pass k - low-pass k-1; the last band is what is left of the input.
        let upper = self.split_buf.get(k).map_or(x, |b| at(b, i));
        let signal = upper - below;
        below = upper;
        let level =
          if self.single_band { at(&self.wide_buf, i) } else { self.detect_buf.get(k).map_or(0.0, |b| at(b, i)) };
        band.peak = band.peak.max(level.abs());
        if tick {
          band.control(self.env_release, self.env_slow, self.window_ticks, self.margin, self.range_db);
        }
        if band.target >= band.gain {
          band.gain += (band.target - band.gain) * self.attack;
          band.hold = self.hold_samples;
        } else if band.hold > 0 {
          band.hold -= 1;
        } else {
          band.gain += (band.target - band.gain) * self.release;
        }
        y += signal * band.gain;
      }
      *sample = if y.is_finite() { y } else { 0.0 };
    }
  }
}

impl BandState {
  /// Once per `CONTROL_SAMPLES`: levels, noise floor, gain target.
  fn control(&mut self, env_release: f32, env_slow: f32, window_ticks: u32, margin_db: f32, range_db: f32) {
    let peak = if self.peak.is_finite() { self.peak } else { 0.0 };
    self.peak = 0.0;
    self.env = peak.max(self.env * env_release);
    self.env_slow += (self.env - self.env_slow) * env_slow;
    self.window_min = self.window_min.min(self.env_slow);
    self.ticks += 1;
    if self.ticks >= window_ticks {
      self.ticks = 0;
      if let Some(slot) = self.mins.get_mut(self.next_min) {
        *slot = self.window_min;
      }
      self.next_min = (self.next_min + 1) % FLOOR_WINDOWS;
      self.window_min = f32::MAX;
    }
    let floor = self.mins.iter().copied().fold(self.window_min, f32::min).clamp(FLOOR_MIN, FLOOR_MAX);
    // dB the level is under "floor + margin": 0 or less = open, KNEE_DB or more = closed.
    let under_db = margin_db - 20.0 * (self.env.max(1e-9) / floor).log10();
    let closed = (under_db / KNEE_DB).clamp(0.0, 1.0);
    self.target = 10f32.powf(-closed * range_db / 20.0);
  }
}

impl SidetoneChain {
  pub fn new(sample_rate: f32) -> Self {
    let rate = if sample_rate.is_finite() && sample_rate >= 8_000.0 { sample_rate } else { 48_000.0 };
    Self {
      rate,
      cuts: BiquadCascade::default(),
      cut_hz: (0.0, 0.0),
      expander: BandExpander::new(rate),
      noise_mode: NoiseMode::Expander,
      level: 0.0,
      level_goal: 0.0,
      level_step: 1.0 / (LEVEL_RAMP_SECONDS * rate),
    }
  }

  /// Takes the settings over; cheap when nothing changed. `mic_volume_gain_db` is the gain of
  /// the Windows mic volume (`mic_input_gain_db_x10`): it is taken back out, so the sidetone
  /// keeps its level when the volume moves. Allocates only when a cut changes (like an EQ edit
  /// in the output APO).
  pub fn configure(&mut self, settings: &SidetoneSettings, mic_volume_gain_db: f32, muted: bool) {
    let s = settings.clamped();
    if (s.low_cut_hz, s.high_cut_hz) != self.cut_hz {
      self.cut_hz = (s.low_cut_hz, s.high_cut_hz);
      let cut = |filter_type: FilterType, frequency: f32| BiquadBand {
        enabled: frequency > 0.0,
        filter_type,
        frequency,
        gain_db: 0.0,
        q: std::f32::consts::FRAC_1_SQRT_2,
        slope_db: 12,
      };
      self
        .cuts
        .set_bands(&[cut(FilterType::HighPass, s.low_cut_hz), cut(FilterType::LowPass, s.high_cut_hz)], self.rate);
    }
    self.expander.set(s.gate_margin_db, s.gate_range_db);
    self.noise_mode = s.noise_mode;
    let volume_db = if mic_volume_gain_db.is_finite() { mic_volume_gain_db } else { 0.0 };
    let compensation_db = -volume_db.clamp(-MIC_VOLUME_COMPENSATION_MAX_DB, MIC_VOLUME_COMPENSATION_MAX_DB);
    self.level_goal = if s.enabled && !muted { 10f32.powf((s.gain_db + compensation_db) / 20.0) } else { 0.0 };
  }

  pub fn noise_mode(&self) -> NoiseMode {
    self.noise_mode
  }

  /// Switched off or muted, and the level has reached zero: nothing to write to the ring.
  pub fn is_silent(&self) -> bool {
    self.level <= 0.0 && self.level_goal <= 0.0
  }

  /// Gain of the most closed expander band, dB (0 while the expander is not the mode).
  pub fn gate_gain_db(&self) -> f32 {
    if self.noise_mode == NoiseMode::Expander {
      self.expander.gain_db()
    } else {
      0.0
    }
  }

  pub fn expander_mut(&mut self) -> &mut BandExpander {
    &mut self.expander
  }

  /// Forgets filter memory and levels: for audio that does not continue the last block.
  pub fn reset(&mut self) {
    self.cuts.reset_state();
    self.expander.reset();
    self.level = 0.0;
  }

  /// The whole chain with the band expander (or without, when the mode is not the expander).
  pub fn process(&mut self, samples: &mut [f32]) {
    self.run_cuts(samples);
    if self.noise_mode == NoiseMode::Expander {
      self.expander.process(samples);
    }
    self.run_level(samples);
  }

  /// Low and high cut. Garbage in the input (NaN, infinity) becomes silence first.
  pub fn run_cuts(&mut self, samples: &mut [f32]) {
    for s in samples.iter_mut() {
      if !s.is_finite() {
        *s = 0.0;
      }
    }
    self.cuts.process(samples);
  }

  /// Sidetone level, moved to its goal in small steps. The last stage: its output is finite.
  pub fn run_level(&mut self, samples: &mut [f32]) {
    for s in samples.iter_mut() {
      let diff = self.level_goal - self.level;
      // Relative steps would never leave zero; a fixed step of the larger of the two does.
      let step = self.level_step * self.level.max(self.level_goal).max(1e-3);
      self.level += diff.clamp(-step, step);
      let y = *s * self.level;
      *s = if y.is_finite() { y.clamp(-SAMPLE_MAX, SAMPLE_MAX) } else { 0.0 };
    }
  }
}

/// Stable 32-bit fingerprint of an endpoint id (FNV-1a over the lower-cased id), never 0:
/// 0 in the shared memory means "no device".
pub fn device_hash(id: &str) -> u32 {
  let hash = id.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b.to_ascii_lowercase())).wrapping_mul(0x0100_0193));
  hash.max(1)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::f32::consts::PI;

  const RATE: u32 = 48_000;
  const OWNER_PID: u32 = 7;

  #[test]
  fn noise_mode_roundtrips_and_an_unknown_one_is_the_expander() {
    for mode in [NoiseMode::Off, NoiseMode::Expander, NoiseMode::WebRtcNs, NoiseMode::RnNoise, NoiseMode::RnNoise2] {
      assert_eq!(NoiseMode::from_u32(mode.to_u32()), mode);
    }
    assert_eq!(NoiseMode::from_u32(99), NoiseMode::Expander);
    assert!(NoiseMode::RnNoise2.is_framed() && !NoiseMode::Expander.is_framed());
  }

  #[test]
  fn settings_are_clamped_and_garbage_becomes_the_default() {
    let s = SidetoneSettings {
      gain_db: 99.0,
      low_cut_hz: 10.0,
      high_cut_hz: f32::NAN,
      gate_margin_db: -5.0,
      gate_range_db: 500.0,
      ..Default::default()
    }
    .clamped();
    assert_eq!(s.gain_db, GAIN_DB_MAX);
    assert_eq!(s.low_cut_hz, LOW_CUT_HZ_MIN);
    assert_eq!(s.high_cut_hz, 0.0, "default: off");
    assert_eq!(s.gate_margin_db, GATE_MARGIN_DB_MIN);
    assert_eq!(s.gate_range_db, GATE_RANGE_DB_MAX);
    assert_eq!(SidetoneSettings { low_cut_hz: 0.0, ..Default::default() }.clamped().low_cut_hz, 0.0, "0 stays off");
  }

  #[test]
  fn device_hash_ignores_case_and_is_never_zero() {
    assert_eq!(device_hash("{0.0.1.00000000}.{ABC}"), device_hash("{0.0.1.00000000}.{abc}"));
    assert_ne!(device_hash("{0.0.1.00000000}.{abc}"), device_hash("{0.0.0.00000000}.{abc}"));
    assert_ne!(device_hash(""), 0);
  }

  #[test]
  fn pairs_roundtrip_ignore_unknown_fields_and_reject_garbage() {
    let mut pairs = SidetonePairs::default();
    let mut pair = SidetonePair::new("mic-A", "out-A", "Mic", "Headphones");
    pair.enabled = true;
    pair.gain_db = -12.5;
    pairs.upsert(pair.clone());
    pairs.upsert(SidetonePair::new("mic-A", "out-B", "Mic", "Other"));
    let back = SidetonePairs::from_json(&pairs.to_json().unwrap()).unwrap();
    assert_eq!(back, pairs);
    assert_eq!(back.find("MIC-a", "OUT-a"), Some(&pair), "ids compare without case");
    assert!(back.find("mic-B", "out-A").is_none());

    // Replacing keeps one entry per pair; removing takes only that pair.
    pair.gain_db = -3.0;
    pairs.upsert(pair);
    assert_eq!(pairs.pairs.len(), 2);
    assert_eq!(pairs.find("mic-A", "out-A").unwrap().gain_db, -3.0);
    pairs.remove("mic-A", "out-B");
    assert_eq!(pairs.pairs.len(), 1);

    // A file from a newer build: unknown fields ignored, missing ones defaulted, unknown mode kept.
    let newer = r#"{"pairs":[{"mic_id":"m","output_id":"o","noise_mode":77,"future":true}],"extra":1}"#;
    let parsed = SidetonePairs::from_json(newer).unwrap();
    let p = parsed.find("m", "o").unwrap();
    assert_eq!((p.noise_mode, p.gain_db, p.enabled), (77, -20.0, false));
    assert_eq!(p.settings().noise_mode, NoiseMode::Expander);
    assert!(SidetonePairs::from_json("{ not json").is_err());
  }

  #[test]
  fn ring_needs_its_owner_and_reads_garbage_as_silence() {
    let ring = SidetoneRing::default();
    let owner = ring.claim_writer(OWNER_PID, 1).unwrap();
    assert!(ring.claim_writer(OWNER_PID, 2).is_none(), "one writer per process");
    let other = ring.claim_writer(OWNER_PID + 1, 1).unwrap();
    ring.write(owner, &[0.5; 4], RATE, 0);
    assert_eq!(ring.write_index(), 0, "the first owner was replaced (another process = stale)");
    ring.write(other, &[0.25, f32::NAN, 100.0, -0.25], RATE, 10_000_000);
    assert_eq!(ring.write_index(), 4);
    assert_eq!([ring.sample(0), ring.sample(1), ring.sample(2), ring.sample(3)], [0.25, 0.0, 0.0, -0.25]);
    // 48 samples behind the head = 1 ms earlier.
    assert_eq!(ring.capture_qpc_hns(48), Some(10_000_000 - 10_000));
    ring.release_writer(other);
    assert!(ring.claim_writer(OWNER_PID, 3).is_some());
  }

  #[test]
  fn reader_plays_the_writer_with_the_same_clock_exactly() {
    let run = simulate(&Sim { seconds: 3.0, ..Sim::default() });
    assert_eq!((run.underruns, run.resyncs), (0, 0));
    // Same rate and no drift: the interpolator sits on whole samples, the output is a copy.
    let tail = &run.out[RATE as usize..];
    assert!(max_step(tail) < TONE_MAX_STEP * 1.01, "no discontinuity");
    let rms = rms(tail);
    assert!((rms - TONE_AMPLITUDE / 2f32.sqrt()).abs() < 0.005, "level kept: {rms}");
  }

  #[test]
  fn reader_follows_a_drifting_clock_without_gaps() {
    for ppm in [-200.0, 200.0] {
      // Mic blocks of 10 ms, output blocks of 2.67 ms, calls up to 0.5 ms late.
      let run =
        simulate(&Sim { seconds: 60.0, writer_ppm: ppm, reader_block: 128, jitter_seconds: 0.0005, ..Sim::default() });
      assert_eq!((run.underruns, run.resyncs), (0, 0), "{ppm} ppm");
      assert!(max_step(&run.out[RATE as usize..]) < TONE_MAX_STEP * 1.2, "{ppm} ppm: no discontinuity");
      // Queue at each call: at most one writer block + one reader block + margin + a little.
      let limit = 480 + 128 + (MARGIN_SECONDS * RATE as f32) as u32 + 96;
      assert!(run.max_queued_late <= limit, "{ppm} ppm: queued {} > {limit}", run.max_queued_late);
    }
  }

  #[test]
  fn reader_moves_up_after_the_start() {
    // Both APOs at 10 ms: after the first window the queue is one block + margin, not the
    // 10 ms extra it started with.
    let run = simulate(&Sim { seconds: 3.0, reader_phase_seconds: 0.004, ..Sim::default() });
    assert_eq!(run.underruns, 0);
    // The writer's block lands 6 ms before each read; the rest is what the reader keeps.
    let limit = 480 + (MARGIN_SECONDS * RATE as f32) as u32 + 96;
    assert!(run.max_queued_late <= limit, "queued {} > {limit}", run.max_queued_late);
  }

  #[test]
  fn reader_converts_the_sample_rate() {
    let run = simulate(&Sim { seconds: 3.0, writer_rate: 44_100, writer_block: 441, ..Sim::default() });
    assert_eq!(run.underruns, 0);
    let tail = &run.out[2 * RATE as usize..];
    let crossings = tail.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
    assert!((crossings as i32 - TONE_HZ as i32).abs() <= 1, "pitch kept: {crossings} Hz");
  }

  #[test]
  fn reader_goes_silent_when_the_writer_stops_and_comes_back() {
    let run = simulate(&Sim { seconds: 4.0, writer_pause: Some((1.5, 2.5)), ..Sim::default() });
    assert_eq!(run.underruns, 1);
    let at = |seconds: f32| (seconds * RATE as f32) as usize;
    assert!(rms(&run.out[at(1.7)..at(2.4)]) == 0.0, "silent while the writer is gone");
    assert!(rms(&run.out[at(2.7)..at(3.9)]) > 0.2, "back after it returned");
  }

  #[test]
  fn reader_drops_what_piled_up_while_it_was_not_called() {
    let ring = SidetoneRing::default();
    let owner = ring.claim_writer(OWNER_PID, 1).unwrap();
    let mut reader = SidetoneReader::default();
    let mut block = [0.0f32; 480];
    let mut step = |reader: &mut SidetoneReader, read: bool| {
      ring.write(owner, &[0.25; 480], RATE, 0);
      read.then(|| reader.read(&ring, &mut block, RATE as f32))
    };
    let playing = (0..50).filter_map(|_| step(&mut reader, true)).last().unwrap();
    assert!(playing.playing);
    // 100 ms without a read (the output engine stood still), then reading again.
    (0..10).for_each(|_| assert!(step(&mut reader, false).is_none()));
    assert!(!step(&mut reader, true).unwrap().playing, "the old queue is not played");
    let again = (0..50).filter_map(|_| step(&mut reader, true)).last().unwrap();
    assert!(again.playing && again.queued < 1_500, "back at a short queue: {}", again.queued);
    assert_eq!(reader.underruns(), 0);

    // Paused on purpose: the same fresh start.
    reader.pause();
    assert!(!step(&mut reader, true).unwrap().playing);
  }

  #[test]
  fn reader_does_not_count_a_writer_that_stopped_on_purpose() {
    let run = simulate(&Sim { seconds: 4.0, writer_pause: Some((1.5, 2.5)), release_in_pause: true, ..Sim::default() });
    assert_eq!(run.underruns, 0, "released ring: no gap counted, the margin stays");
    let at = |seconds: f32| (seconds * RATE as f32) as usize;
    assert!(rms(&run.out[at(1.7)..at(2.4)]) == 0.0);
    assert!(rms(&run.out[at(2.7)..at(3.9)]) > 0.2, "back after the writer returned");
  }

  #[test]
  fn expander_turns_steady_noise_down_by_its_depth() {
    for single in [false, true] {
      let mut expander = BandExpander::new(RATE as f32);
      expander.set(10.0, 18.0);
      expander.set_single_band(single);
      let input = noise(6 * RATE as usize, 0.001, 3);
      let mut out = input.clone();
      process_in_blocks(&mut out, |b| expander.process(b));
      let cut = db(rms(&out[4 * RATE as usize..]) / rms(&input[4 * RATE as usize..]));
      assert!((-19.0..=-16.0).contains(&cut), "single={single}: noise changed by {cut:.1} dB");
    }
  }

  #[test]
  fn expander_opens_for_a_voice_within_two_milliseconds() {
    let mut expander = BandExpander::new(RATE as f32);
    let onset = 3 * RATE as usize;
    let mut input = noise(4 * RATE as usize, 0.001, 5);
    for (i, s) in input.iter_mut().enumerate().skip(onset) {
      *s += voice(i - onset, 0.03);
    }
    let mut out = input.clone();
    process_in_blocks(&mut out, |b| expander.process(b));
    let ms = RATE as usize / 1000;
    let early = db(rms(&out[onset + 2 * ms..onset + 20 * ms]) / rms(&input[onset + 2 * ms..onset + 20 * ms]));
    let steady = db(rms(&out[onset + 100 * ms..]) / rms(&input[onset + 100 * ms..]));
    assert!(early > -1.5, "2-20 ms after the onset: {early:.1} dB");
    assert!(steady > -0.5, "steady voice: {steady:.1} dB");
  }

  #[test]
  fn expander_takes_hiss_from_under_a_low_tone_only_in_bands() {
    // Hiss above 6 kHz at -60 dBFS under a 200 Hz tone at -20 dBFS.
    let len = 6 * RATE as usize;
    let mut hiss = noise(len, 0.004, 9);
    high_pass(&mut hiss, 6_000.0);
    let tone: Vec<f32> = (0..len).map(|i| 0.1 * (2.0 * PI * 200.0 * i as f32 / RATE as f32).sin()).collect();
    let input: Vec<f32> = hiss.iter().zip(&tone).map(|(h, t)| h + t).collect();
    let tail = 4 * RATE as usize;
    let hiss_left = |single: bool| {
      let mut expander = BandExpander::new(RATE as f32);
      expander.set_single_band(single);
      let mut out = input.clone();
      process_in_blocks(&mut out, |b| expander.process(b));
      let tone_change = db(rms(&out[tail..]) / rms(&input[tail..]));
      // The bands overlap gently: a closed neighbour takes a little of the tone with it.
      assert!(tone_change.abs() < 1.0, "single={single}: tone changed by {tone_change:.2} dB");
      let (mut a, mut b) = (out, input.clone());
      high_pass(&mut a, 4_000.0);
      high_pass(&mut b, 4_000.0);
      db(rms(&a[tail..]) / rms(&b[tail..]))
    };
    let (bands, single) = (hiss_left(false), hiss_left(true));
    assert!(bands < -12.0, "bands: hiss under the tone changed by {bands:.1} dB");
    assert!(single > -1.0, "one band: the tone holds it open ({single:.1} dB)");
  }

  #[test]
  fn expander_with_no_depth_returns_the_input() {
    let mut expander = BandExpander::new(RATE as f32);
    expander.set(10.0, 0.0);
    let input: Vec<f32> = noise(RATE as usize, 0.1, 11).iter().enumerate().map(|(i, n)| n + voice(i, 0.1)).collect();
    let mut out = input.clone();
    process_in_blocks(&mut out, |b| expander.process(b));
    // Gains start at 0 and open within a few ms: compare after that.
    let skip = RATE as usize / 10;
    let diff: Vec<f32> = out.iter().zip(&input).skip(skip).map(|(a, b)| a - b).collect();
    let error = db(rms(&diff) / rms(&input[skip..]));
    assert!(error < -90.0, "bands do not add up to the input: {error:.0} dB");
  }

  #[test]
  fn chain_survives_garbage_and_ramps_its_level() {
    let mut chain = SidetoneChain::new(RATE as f32);
    let on = SidetoneSettings {
      enabled: true,
      gain_db: 0.0,
      noise_mode: NoiseMode::Off,
      low_cut_hz: 0.0,
      ..Default::default()
    };
    chain.configure(&on, 0.0, false);
    assert!(!chain.is_silent());
    let mut block = vec![0.5f32; 4_800];
    block[10] = f32::NAN;
    block[11] = f32::INFINITY;
    chain.process(&mut block);
    assert!(block.iter().all(|s| s.is_finite()));
    assert!(max_step(&block[100..]) < 0.01, "the level ramps, no jump");
    assert!((block[4_799] - 0.5).abs() < 1e-3, "0 dB reached: {}", block[4_799]);

    // The Windows mic volume at -6 dB is taken back out: +6 dB.
    chain.configure(&on, -6.0, false);
    let mut block = vec![0.25f32; 4_800];
    chain.process(&mut block);
    assert!((block[4_799] - 0.25 * 10f32.powf(6.0 / 20.0)).abs() < 1e-3);

    // Muted: down to exactly zero, then nothing to write.
    chain.configure(&on, 0.0, true);
    let mut block = vec![0.5f32; 4_800];
    chain.process(&mut block);
    assert!(max_step(&block) < 0.01);
    assert_eq!(block[4_799], 0.0);
    assert!(chain.is_silent());
  }

  #[test]
  fn chain_low_cut_removes_rumble() {
    let mut chain = SidetoneChain::new(RATE as f32);
    chain.configure(
      &SidetoneSettings { enabled: true, gain_db: 0.0, noise_mode: NoiseMode::Off, ..Default::default() },
      0.0,
      false,
    );
    let tone =
      |hz: f32| (0..RATE as usize).map(|i| 0.5 * (2.0 * PI * hz * i as f32 / RATE as f32).sin()).collect::<Vec<f32>>();
    let mut gain = |hz: f32| {
      let input = tone(hz);
      let mut out = input.clone();
      chain.process(&mut out);
      db(rms(&out[RATE as usize / 2..]) / rms(&input[RATE as usize / 2..]))
    };
    assert!(gain(50.0) < -20.0);
    assert!(gain(1_000.0).abs() < 0.5);
  }

  // --- helpers ---

  const TONE_HZ: f32 = 440.0;
  const TONE_AMPLITUDE: f32 = 0.5;
  /// Largest sample-to-sample step of the test tone at 48 kHz.
  const TONE_MAX_STEP: f32 = 2.0 * PI * TONE_HZ / RATE as f32 * TONE_AMPLITUDE;

  /// A writer and a reader on their own clocks, in simulated time.
  struct Sim {
    seconds: f64,
    writer_rate: u32,
    writer_block: usize,
    /// The writer's clock runs this much fast.
    writer_ppm: f64,
    reader_block: usize,
    /// First reader call this long after the first writer block.
    reader_phase_seconds: f64,
    /// Reader calls come up to this late.
    jitter_seconds: f64,
    /// The writer delivers nothing between these two times.
    writer_pause: Option<(f64, f64)>,
    /// The writer gives the ring up at the start of the pause and takes it again after it.
    release_in_pause: bool,
  }

  impl Default for Sim {
    fn default() -> Self {
      Self {
        seconds: 1.0,
        writer_rate: RATE,
        writer_block: 480,
        writer_ppm: 0.0,
        reader_block: 480,
        reader_phase_seconds: 0.0013,
        jitter_seconds: 0.0,
        writer_pause: None,
        release_in_pause: false,
      }
    }
  }

  struct Run {
    out: Vec<f32>,
    underruns: u32,
    resyncs: u32,
    /// Largest queue seen by a call after the first second.
    max_queued_late: u32,
  }

  fn simulate(sim: &Sim) -> Run {
    let ring = SidetoneRing::default();
    let mut owner = ring.claim_writer(OWNER_PID, 1);
    let mut reader = SidetoneReader::default();
    let writer_period = sim.writer_block as f64 / (f64::from(sim.writer_rate) * (1.0 + sim.writer_ppm * 1e-6));
    let reader_period = sim.reader_block as f64 / f64::from(RATE);
    let (mut next_write, mut next_read) = (0.0, sim.reader_phase_seconds);
    let (mut written, mut reads) = (0u64, 0u64);
    let mut seed = 1u32;
    let mut out = Vec::new();
    let mut max_queued_late = 0;
    let mut block = vec![0.0f32; sim.reader_block];
    while next_read < sim.seconds {
      if next_write <= next_read {
        let paused = sim.writer_pause.is_some_and(|(from, to)| next_write >= from && next_write < to);
        if paused && sim.release_in_pause {
          if let Some(owner) = owner.take() {
            ring.release_writer(owner);
          }
        }
        if !paused {
          let owner = *owner.get_or_insert_with(|| ring.claim_writer(OWNER_PID, 1).unwrap());
          let samples: Vec<f32> = (0..sim.writer_block)
            .map(|i| {
              TONE_AMPLITUDE
                * (2.0 * PI * TONE_HZ * ((written + i as u64) % u64::from(sim.writer_rate)) as f32
                  / sim.writer_rate as f32)
                  .sin()
            })
            .collect();
          ring.write(owner, &samples, sim.writer_rate, 0);
        }
        // The tone keeps its phase through a pause (the samples are lost, not delayed).
        written += sim.writer_block as u64;
        next_write += writer_period;
      } else {
        let info = reader.read(&ring, &mut block, RATE as f32);
        if next_read > 1.0 && info.playing {
          max_queued_late = max_queued_late.max(info.queued + sim.reader_block as u32);
        }
        out.extend_from_slice(&block);
        reads += 1;
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let late = f64::from(seed >> 8) / f64::from(1u32 << 24) * sim.jitter_seconds;
        next_read = sim.reader_phase_seconds + reads as f64 * reader_period + late;
      }
    }
    Run { out, underruns: reader.underruns(), resyncs: reader.resyncs(), max_queued_late }
  }

  fn process_in_blocks(samples: &mut [f32], mut f: impl FnMut(&mut [f32])) {
    samples.chunks_mut(480).for_each(&mut f);
  }

  /// White noise, uniform in +-`amplitude`.
  fn noise(len: usize, amplitude: f32, seed: u32) -> Vec<f32> {
    let mut state = seed;
    (0..len)
      .map(|_| {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        amplitude * ((state >> 8) as f32 / 8_388_608.0 - 1.0)
      })
      .collect()
  }

  /// Harmonics of 150 Hz up to 6 kHz, falling 6 dB per octave.
  fn voice(i: usize, amplitude: f32) -> f32 {
    let t = i as f32 / RATE as f32;
    (1..=40).map(|h| amplitude / h as f32 * (2.0 * PI * 150.0 * h as f32 * t).sin()).sum()
  }

  fn high_pass(samples: &mut [f32], hz: f32) {
    let mut filter = BiquadCascade::default();
    let band = BiquadBand {
      enabled: true,
      filter_type: FilterType::HighPass,
      frequency: hz,
      gain_db: 0.0,
      q: 0.707,
      slope_db: 48,
    };
    filter.set_bands(&[band], RATE as f32);
    filter.process(samples);
  }

  fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / samples.len().max(1) as f64).sqrt() as f32
  }

  fn db(ratio: f32) -> f32 {
    20.0 * ratio.max(1e-9).log10()
  }

  fn max_step(samples: &[f32]) -> f32 {
    samples.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
  }
}
