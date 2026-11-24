//! Combining the layers into the band lists and preamps sent to the APOs.

use super::*;

/// Loudness matching: the average gain in this range (where hearing is most sensitive) is
/// compensated instead of the highest peak.
const LOUDNESS_BAND_HZ: (f32, f32) = (200.0, 6000.0);

const PREAMP_RANGE_DB: (f32, f32) = (-30.0, 12.0);

/// Mean gain (dB) of the combined curve over the loudness band.
fn average_gain(result: &CombinedEngineResult) -> f32 {
  let c = &result.combined_curve;
  let (sum, n) = c
    .frequencies
    .iter()
    .zip(&c.gains_db)
    .filter(|(f, _)| (LOUDNESS_BAND_HZ.0..=LOUDNESS_BAND_HZ.1).contains(*f))
    .fold((0.0f32, 0usize), |(s, n), (_, g)| (s + g, n + 1));
  if n == 0 { 0.0 } else { sum / n as f32 }
}

impl OutputEq {
  /// Headphone + channel profile curves, volume change and peak after it, i.e. what a
  /// paired chat/media app sounds like. Cached per channel until the next `recompute`.
  pub fn bus_result(&mut self, bus: StreamBus) -> (&CombinedEngineResult, f32, f32) {
    let missing = match bus {
      StreamBus::Chat => self.chat_result.is_none(),
      StreamBus::Media => self.media_result.is_none(),
      _ => return (&self.result, self.preamp_db, self.peak_after_db),
    };
    if missing {
      let (hp, own) = self.bus_layers(bus);
      let computed = self.volume_change(hp, own);
      if bus == StreamBus::Chat {
        self.chat_result = Some(computed);
      } else {
        self.media_result = Some(computed);
      }
    }
    let cached = if bus == StreamBus::Chat { self.chat_result.as_ref() } else { self.media_result.as_ref() };
    match cached {
      Some((result, preamp, peak)) => (result, *preamp, *peak),
      None => (&self.result, self.preamp_db, self.peak_after_db),
    }
  }

  /// Recompute curves and publish the combined EQ + master switch.
  pub fn apply(&mut self, shm: Option<&ShmHeader>) {
    self.recompute();
    let Some(h) = shm else {
      return;
    };
    let s = &self.settings;
    let enabled_stages = |p: Option<&ParametricProfile>| -> Vec<BiquadBand> {
      expand_slopes(&p.map(|p| p.bands.clone()).unwrap_or_default()).into_iter().filter(|b| b.enabled).collect()
    };
    let hp = enabled_stages(self.headphone_profile().filter(|_| s.enabled && s.headphone_enabled));
    let game = enabled_stages(self.game_profile().filter(|_| s.enabled && s.game_enabled));
    // Stages as the output APO runs them: headphone first, then game, up to the slot count.
    let mut bands: Vec<BiquadBand> = hp.iter().chain(&game).cloned().collect();
    if bands.len() > MAX_OUT_EQ_BANDS {
      warn!("[OutputEq] {} filter stages, only the first {MAX_OUT_EQ_BANDS} are applied", bands.len());
      bands.truncate(MAX_OUT_EQ_BANDS);
    }
    let enabled = s.enabled;
    let preamp = if enabled { self.preamp_db } else { 0.0 };
    h.master_bypass.store(!enabled, Ordering::Relaxed);
    h.write_out_eq(&bands, preamp);

    // The stream EQ undoes exactly the game stages the output APO got.
    let game_applied = bands.get(hp.len()..).unwrap_or_default();
    for (bus, slot) in StreamBus::WITH_EQ.iter().zip(&h.stream_eq) {
      let (stages, bus_preamp) = if enabled { self.stream_eq(*bus, game_applied, preamp) } else { (Vec::new(), 0.0) };
      slot.write(&stages, bus_preamp);
    }
  }

  pub(super) fn notify(&mut self, shm: Option<&ShmHeader>) {
    if let Some(h) = shm {
      let v = h.out_settings_version.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
      self.seen_settings_version = Some(v);
    }
  }

  /// Curves + the volume change of headphone + game.
  pub(super) fn recompute(&mut self) {
    let s = &self.settings;
    let hp = if s.enabled && s.headphone_enabled { self.headphone_profile() } else { None };
    let game = if s.enabled && s.game_enabled { self.game_profile() } else { None };
    let (result, preamp, peak) = self.volume_change(hp, game);
    (self.result, self.preamp_db, self.peak_after_db) = (result, preamp, peak);
    self.chat_result = None;
    self.media_result = None;
  }

  /// Curves of `hp` + `layer`, their volume change (automatic preamp, safe or
  /// loudness-matched, plus both profiles' own gains) and the peak after it.
  pub(super) fn volume_change(
    &self,
    hp: Option<&ParametricProfile>,
    layer: Option<&ParametricProfile>,
  ) -> (CombinedEngineResult, f32, f32) {
    let bands = |p: Option<&ParametricProfile>| p.map(|p| p.bands.clone()).unwrap_or_default();
    let (hp_bands, layer_bands) = (bands(hp), bands(layer));
    let user_gain = hp.map_or(0.0, |p| p.gain_db) + layer.map_or(0.0, |p| p.gain_db);
    let has_eq = hp_bands.iter().chain(&layer_bands).any(|b| b.enabled);
    let result = compute_combined_engine(&hp_bands, &layer_bands, 48_000.0);
    let max_boost = result.max_boost_db;
    let auto = match (has_eq, self.settings.loudness_match) {
      (false, _) => 0.0,
      (true, false) => result.auto_preamp_db,
      (true, true) => (-average_gain(&result)).min(MATCH_MAX_PEAK_DB - max_boost),
    };
    let preamp = (auto + user_gain).clamp(PREAMP_RANGE_DB.0, PREAMP_RANGE_DB.1);
    (result, preamp, max_boost + preamp)
  }

  /// (headphone, channel profile) a chat/media stream should end up with.
  fn bus_layers(&self, bus: StreamBus) -> (Option<&ParametricProfile>, Option<&ParametricProfile>) {
    let s = &self.settings;
    let hp = if s.enabled && s.headphone_enabled { self.headphone_profile() } else { None };
    let own = if s.enabled && self.bus_enabled(bus) { self.bus_profile(bus) } else { None };
    (hp, own)
  }

  /// Stream EQ of a chat/media stream: its own profile, then the game stages undone; the
  /// preamp turns the output's volume change into the channel's own.
  fn stream_eq(&mut self, bus: StreamBus, game_applied: &[BiquadBand], out_preamp_db: f32) -> (Vec<BiquadBand>, f32) {
    let (_, own) = self.bus_layers(bus);
    let mut stages: Vec<BiquadBand> =
      expand_slopes(&own.map(|p| p.bands.clone()).unwrap_or_default()).into_iter().filter(|b| b.enabled).collect();
    stages.extend(invert_bands(game_applied));
    if stages.len() > MAX_STREAM_EQ_BANDS {
      warn!("[OutputEq] {bus:?} stream EQ: {} stages, only the first {MAX_STREAM_EQ_BANDS} are applied", stages.len());
      stages.truncate(MAX_STREAM_EQ_BANDS);
    }
    let own_preamp = self.bus_result(bus).1;
    (stages, own_preamp - out_preamp_db)
  }
}
