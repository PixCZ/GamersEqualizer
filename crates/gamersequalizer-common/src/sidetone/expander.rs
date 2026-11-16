//! The band expander: the sidetone's noise suppression without delay.

use super::*;

/// Band edges of the expander: below 500 Hz, 500 Hz - 2 kHz, 2 - 5 kHz, above 5 kHz.
const BAND_EDGES_HZ: [f32; 3] = [500.0, 2_000.0, 5_000.0];
const BANDS: usize = BAND_EDGES_HZ.len() + 1;
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
/// After a start the slow level itself is the floor for this long (it has not settled, and
/// its first values would otherwise sit in the minimum for seconds).
const FLOOR_WARM_UP_SECONDS: f32 = 0.3;
/// The floor stays inside -90..-30 dBFS: digital silence must not pull it to nothing, and a
/// long loud sound must not push the threshold above a voice.
const FLOOR_MIN: f32 = 3.16e-5;
const FLOOR_MAX: f32 = 0.0316;
/// A band goes from closed to open over this many dB below "floor + margin".
const KNEE_DB: f32 = 4.0;
const ATTACK_SECONDS: f32 = 0.001;
const HOLD_SECONDS: f32 = 0.150;
const RELEASE_SECONDS: f32 = 0.150;
/// Downward expander in `BANDS` bands. Each band measures its own noise floor in
/// the pauses and is turned down whenever nothing louder is in it.
///
/// The bands are differences of low-passes (band k = LP k - LP k-1, the last one input - LP),
/// so with all gains at 1 their sum is the input again. The levels come from separate, steep
/// band-passes: a loud vowel must not open the hiss band.
#[derive(Debug, Clone)]
pub struct BandExpander {
  split: [BiquadCascade; BANDS - 1],
  detect: [BiquadCascade; BANDS],
  bands: [BandState; BANDS],
  phase: u32,
  rates: ControlRates,
  attack: f32,
  release: f32,
  hold_samples: u32,
  margin: f32,
  range_db: f32,
  split_buf: [[f32; CHUNK]; BANDS - 1],
  detect_buf: [[f32; CHUNK]; BANDS],
}

/// What `BandState::control` needs, per control tick.
#[derive(Debug, Clone, Copy)]
struct ControlRates {
  env_release: f32,
  env_slow: f32,
  window_ticks: u32,
  warm_up_ticks: u32,
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
  /// Control ticks left of the warm-up; None = no level seen yet.
  warm_up: Option<u32>,
  target: f32,
  gain: f32,
  hold: u32,
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
      warm_up: None,
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
      bands: [BandState::new(); BANDS],
      phase: 0,
      rates: ControlRates {
        env_release: (-tick / ENV_RELEASE_SECONDS).exp(),
        env_slow: 1.0 - (-tick / ENV_SLOW_SECONDS).exp(),
        window_ticks: (FLOOR_WINDOW_SECONDS / tick) as u32,
        warm_up_ticks: (FLOOR_WARM_UP_SECONDS / tick) as u32,
      },
      attack: per_sample(ATTACK_SECONDS),
      release: per_sample(RELEASE_SECONDS),
      hold_samples: (HOLD_SECONDS * rate) as u32,
      margin: 10.0,
      range_db: 18.0,
      split_buf: [[0.0; CHUNK]; BANDS - 1],
      detect_buf: [[0.0; CHUNK]; BANDS],
    }
  }

  /// Sensitivity (a band opens this many dB above its floor) and depth, both in dB.
  pub fn set(&mut self, margin_db: f32, range_db: f32) {
    self.margin = margin_db.clamp(GATE_MARGIN_DB_MIN, GATE_MARGIN_DB_MAX);
    self.range_db = range_db.clamp(0.0, GATE_RANGE_DB_MAX);
  }

  /// Gain of the most closed band right now, dB (0 = all open).
  pub fn gain_db(&self) -> f32 {
    let min = self.bands.iter().map(|b| b.gain).fold(1.0f32, f32::min);
    20.0 * min.max(1e-4).log10()
  }

  /// For audio that does not continue the last block (after a pause): forgets the filter
  /// memory and closes the bands. The noise floors stay: it is still the same microphone.
  pub fn reset(&mut self) {
    self.split.iter_mut().chain(self.detect.iter_mut()).for_each(BiquadCascade::reset_state);
    for band in self.bands.iter_mut() {
      (band.peak, band.env, band.target, band.gain, band.hold) = (0.0, 0.0, 0.0, 0.0, 0);
    }
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
    self.detect.iter_mut().zip(self.detect_buf.iter_mut()).for_each(|(c, b)| filter(c, b));

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
        let level = self.detect_buf.get(k).map_or(0.0, |b| at(b, i));
        band.peak = band.peak.max(level.abs());
        if tick {
          band.control(&self.rates, self.margin, self.range_db);
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
  fn control(&mut self, c: &ControlRates, margin_db: f32, range_db: f32) {
    let peak = if self.peak.is_finite() { self.peak } else { 0.0 };
    self.peak = 0.0;
    self.env = peak.max(self.env * c.env_release);
    match self.warm_up {
      // The first level: the slow one starts there, not at zero.
      None => {
        self.env_slow = self.env;
        self.warm_up = Some(c.warm_up_ticks);
      }
      Some(left) => {
        self.env_slow += (self.env - self.env_slow) * c.env_slow;
        self.warm_up = Some(left.saturating_sub(1));
      }
    }
    if self.warm_up.is_some_and(|left| left > 0) {
      // Not settled yet: the floor is the slow level itself, so the band stays closed
      // unless something clearly louder than the average comes.
      self.window_min = self.env_slow;
      self.ticks = 0;
    } else {
      self.window_min = self.window_min.min(self.env_slow);
      self.ticks += 1;
    }
    if self.ticks >= c.window_ticks {
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
