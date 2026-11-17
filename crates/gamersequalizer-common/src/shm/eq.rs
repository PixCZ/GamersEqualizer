//! EQ bands in shared memory: mic, sidetone, output and the per-channel stream EQs.

use super::*;

/// One parametric EQ band in shared memory (f32 values stored as bits).
#[repr(C)]
#[derive(Default)]
pub struct ShmEqBand {
  pub enabled: AtomicU32,
  pub filter_type: AtomicU32,
  pub frequency_bits: AtomicU32,
  pub gain_bits: AtomicU32,
  pub q_bits: AtomicU32,
}

/// EQ the stream APO runs on one channel's app streams (chat or media), before the mix.
#[repr(C)]
pub struct ShmStreamEq {
  pub version: AtomicU32,
  pub band_count: AtomicU32,
  pub preamp_db_bits: AtomicU32,
  pub bands: [ShmEqBand; MAX_STREAM_EQ_BANDS],
}

impl Default for ShmStreamEq {
  fn default() -> Self {
    Self {
      version: AtomicU32::new(0),
      band_count: AtomicU32::new(0),
      preamp_db_bits: AtomicU32::new(0),
      bands: std::array::from_fn(|_| ShmEqBand::default()),
    }
  }
}

impl ShmStreamEq {
  /// Publish; the APO picks it up when `version` changes. Empty + 0 dB = pass-through.
  pub fn write(&self, bands: &[BiquadBand], preamp_db: f32) {
    let count = write_bands(&self.bands, bands);
    self.band_count.store(count as u32, Ordering::Relaxed);
    self.preamp_db_bits.store(preamp_db.to_bits(), Ordering::Relaxed);
    self.version.fetch_add(1, Ordering::Release);
  }

  /// Bands + preamp (dB); a non-finite preamp reads as 0 dB.
  pub fn read(&self) -> (impl Iterator<Item = BiquadBand> + '_, f32) {
    let count = (self.band_count.load(Ordering::Acquire) as usize).min(MAX_STREAM_EQ_BANDS);
    let preamp = f32::from_bits(self.preamp_db_bits.load(Ordering::Relaxed));
    let preamp = if preamp.is_finite() { preamp.clamp(-48.0, 24.0) } else { 0.0 };
    (read_bands(&self.bands, count), preamp)
  }
}

/// Returns how many bands were stored (bounded by the slot count).
fn write_bands(slots: &[ShmEqBand], bands: &[BiquadBand]) -> usize {
  // The engine runs plain biquads: a steep cut takes several slots.
  let bands = expand_slopes(bands);
  let count = bands.len().min(slots.len());
  for (slot, band) in slots.iter().zip(bands.iter().take(count)) {
    slot.enabled.store(band.enabled as u32, Ordering::Relaxed);
    slot.filter_type.store(band.filter_type.to_u32(), Ordering::Relaxed);
    slot.frequency_bits.store(band.frequency.to_bits(), Ordering::Relaxed);
    slot.gain_bits.store(band.gain_db.to_bits(), Ordering::Relaxed);
    slot.q_bits.store(band.q.to_bits(), Ordering::Relaxed);
  }
  count
}

/// Whether `slots` (with `count` in use) already hold what `write_bands` would store for `bands`.
fn same_bands(slots: &[ShmEqBand], count: usize, bands: &[BiquadBand]) -> bool {
  let bands = expand_slopes(bands);
  bands.len().min(slots.len()) == count
    && slots.iter().zip(bands.iter()).take(count).all(|(slot, band)| {
      slot.enabled.load(Ordering::Relaxed) == band.enabled as u32
        && slot.filter_type.load(Ordering::Relaxed) == band.filter_type.to_u32()
        && slot.frequency_bits.load(Ordering::Relaxed) == band.frequency.to_bits()
        && slot.gain_bits.load(Ordering::Relaxed) == band.gain_db.to_bits()
        && slot.q_bits.load(Ordering::Relaxed) == band.q.to_bits()
    })
}

/// Bands with an unknown filter type or non-finite values are skipped; the rest clamped.
/// Lazy, so the APOs build their EQ from it on the audio thread without allocating.
fn read_bands(slots: &[ShmEqBand], count: usize) -> impl Iterator<Item = BiquadBand> + '_ {
  slots.iter().take(count).filter_map(|slot| {
    let filter_type = FilterType::from_u32(slot.filter_type.load(Ordering::Relaxed))?;
    let mut band = BiquadBand {
      enabled: slot.enabled.load(Ordering::Relaxed) != 0,
      filter_type,
      frequency: f32::from_bits(slot.frequency_bits.load(Ordering::Relaxed)),
      gain_db: f32::from_bits(slot.gain_bits.load(Ordering::Relaxed)),
      q: f32::from_bits(slot.q_bits.load(Ordering::Relaxed)),
      slope_db: 12,
    };
    if !(band.frequency.is_finite() && band.gain_db.is_finite() && band.q.is_finite()) {
      return None;
    }
    band.clamp_parameters();
    Some(band)
  })
}

impl ShmHeader {
  /// Publish the combined output EQ. Readers pick it up when `out_eq_version` changes.
  pub fn write_out_eq(&self, bands: &[BiquadBand], preamp_db: f32) {
    let count = write_bands(&self.out_eq_bands, bands);
    self.out_eq_band_count.store(count as u32, Ordering::Relaxed);
    self.out_preamp_db_bits.store(preamp_db.to_bits(), Ordering::Relaxed);
    self.out_eq_version.fetch_add(1, Ordering::Release);
  }

  /// Output EQ bands + preamp (dB). A non-finite preamp reads as 0 dB.
  pub fn read_out_eq(&self) -> (impl Iterator<Item = BiquadBand> + '_, f32) {
    let count = (self.out_eq_band_count.load(Ordering::Acquire) as usize).min(MAX_OUT_EQ_BANDS);
    let preamp = f32::from_bits(self.out_preamp_db_bits.load(Ordering::Relaxed));
    let preamp = if preamp.is_finite() { preamp.clamp(-48.0, 24.0) } else { 0.0 };
    (read_bands(&self.out_eq_bands, count), preamp)
  }

  /// True when the stream APO changes some chat / media stream (not bypassed, a bus not flat).
  /// False = every stream passes through, so pairing streams with apps is pointless.
  pub fn stream_eq_active(&self) -> bool {
    !self.master_bypass.load(Ordering::Relaxed)
      && self.stream_eq.iter().any(|eq| {
        let (mut bands, preamp) = eq.read();
        bands.next().is_some() || preamp != 0.0
      })
  }

  /// Publish the sidetone EQ of the current pair. The mic APO picks it up when
  /// `sidetone_eq_version` changes.
  pub fn write_sidetone_eq(&self, bands: &[BiquadBand]) {
    let count = write_bands(&self.sidetone_eq_bands, bands);
    self.sidetone_eq_band_count.store(count as u32, Ordering::Relaxed);
    self.sidetone_eq_version.fetch_add(1, Ordering::Release);
  }

  /// Read the sidetone EQ. Bands with an unknown filter type are skipped.
  pub fn read_sidetone_eq(&self) -> impl Iterator<Item = BiquadBand> + '_ {
    let count = (self.sidetone_eq_band_count.load(Ordering::Acquire) as usize).min(MAX_EQ_BANDS);
    read_bands(&self.sidetone_eq_bands, count)
  }

  /// Publish the microphone EQ. Readers pick it up when `mic_eq_version` changes.
  pub fn write_mic_eq(&self, bands: &[BiquadBand]) {
    // The same bands again (the window publishes them on every start) keep the version: the mic
    // chain counts a new one as a changed setting and the leveler would learn fast again.
    let in_use = self.mic_eq_band_count.load(Ordering::Acquire) as usize;
    if same_bands(&self.mic_eq_bands, in_use, bands) {
      return;
    }
    let count = write_bands(&self.mic_eq_bands, bands);
    self.mic_eq_band_count.store(count as u32, Ordering::Relaxed);
    self.mic_eq_version.fetch_add(1, Ordering::Release);
  }

  /// Read the microphone EQ. Bands with an unknown filter type are skipped.
  pub fn read_mic_eq(&self) -> impl Iterator<Item = BiquadBand> + '_ {
    let count = (self.mic_eq_band_count.load(Ordering::Acquire) as usize).min(MAX_EQ_BANDS);
    read_bands(&self.mic_eq_bands, count)
  }
}
