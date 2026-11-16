//! The output EQ processor, which also mixes the sidetone in.

use super::*;

/// Output EQ: shared coefficients with per-channel state, for the stream's sample rate.
pub(super) struct OutputProcessor {
  shm: Option<SharedMemoryClient>,
  sample_rate: f32,
  eq: EqFade,
  /// Catches peaks above 0 dBFS from loudness matching / user gain.
  limiter: PeakLimiter,
  /// `out_eq_version` the cascade was built from.
  version: Option<u32>,
  /// Anything to do (bands or a preamp); false = pass through.
  active: bool,
  owner: OwnerWatch,
  failed: bool,
  /// Sidetone: reader of the mic APO's ring, its mono block, and this endpoint's hash
  /// (0 = unknown: never plays).
  sidetone: SidetoneReader,
  sidetone_buf: Vec<f32>,
  pub(super) sidetone_endpoint: u32,
  /// Sidetone was mixed into the previous block.
  sidetone_playing: bool,
}

impl OutputProcessor {
  /// None (pass-through) only if building panicked. Without the mapping it passes through.
  pub(super) fn create(shm: Option<SharedMemoryClient>, sample_rate: f32, channels: u32) -> Option<Self> {
    catch_unwind(AssertUnwindSafe(move || OutputProcessor {
      shm,
      sample_rate,
      // ~85 ms of stereo at 48 kHz for the fade's copy; grows only if the engine sends longer blocks.
      eq: EqFade::new(BiquadCascade::with_channels(channels as usize), sample_rate, 8192),
      limiter: PeakLimiter::new(sample_rate),
      version: None,
      active: false,
      owner: OwnerWatch::default(),
      failed: false,
      sidetone: SidetoneReader::default(),
      sidetone_buf: Vec::with_capacity(4096),
      sidetone_endpoint: 0,
      sidetone_playing: false,
    }))
    .ok()
  }

  /// In place on interleaved `buf`: preamp, then each channel through its own cascade.
  pub(super) fn process(&mut self, buf: &mut [f32], channels: usize) {
    if self.failed || channels == 0 {
      return;
    }
    let ran = catch_unwind(AssertUnwindSafe(|| self.process_inner(buf, channels)));
    if ran.is_err() {
      self.failed = true;
    }
  }

  fn process_inner(&mut self, buf: &mut [f32], channels: usize) {
    let Some(shm) = self.shm.as_ref() else {
      return;
    };
    let layout = shm.layout();
    let h: &ShmHeader = &layout.header;
    h.out_apo_blocks.fetch_add(1, Ordering::Relaxed);
    h.out_apo_sample_rate.store(self.sample_rate as u32, Ordering::Relaxed);
    h.out_apo_channels.store(channels as u32, Ordering::Relaxed);
    // Nobody's settings to apply (app not running for the user at the console): untouched.
    let enabled = self.owner.live(h, Instant::now()) && !h.master_bypass.load(Ordering::Relaxed);
    if enabled {
      let version = h.out_eq_version.load(Ordering::Acquire);
      if self.version != Some(version) {
        self.version = Some(version);
        let (bands, preamp_db) = h.read_out_eq();
        self.eq.cascade.set_bands(bands, self.sample_rate);
        self.eq.set_preamp_db(preamp_db);
        self.active = !self.eq.cascade.is_empty() || preamp_db != 0.0;
      }
    }
    let goal = if enabled && self.active { 1.0 } else { 0.0 };
    // The mono sidetone block for this buffer is in `sidetone_buf` when this is true.
    let sidetone = read_sidetone(
      &mut self.sidetone,
      &mut self.sidetone_buf,
      layout,
      buf.len() / channels,
      self.sample_rate,
      self.sidetone_endpoint,
    );
    let sidetone_started = sidetone && !std::mem::replace(&mut self.sidetone_playing, sidetone);
    if self.eq.wet <= 0.0 && goal <= 0.0 {
      if sidetone {
        if sidetone_started {
          // The limiter did not run while nothing was mixed in: its gain is from back then.
          self.limiter = PeakLimiter::new(self.sample_rate);
        }
        // No EQ, but the sum of the mix and the voice still must not clip.
        add_sidetone(buf, channels, &self.sidetone_buf);
        self.limiter.process(buf, channels);
      }
      return;
    }
    if self.eq.wet <= 0.0 {
      // Fading in after a pass-through stretch: the filter memory is from back then.
      self.eq.cascade.reset_state();
    }
    if self.eq.wet >= 1.0 && goal >= 1.0 {
      // After the EQ (the voice gets no game profile), before the limiter (it guards the sum).
      self.eq.run(buf, channels);
      if sidetone {
        add_sidetone(buf, channels, &self.sidetone_buf);
      }
      self.limiter.process(buf, channels);
      return;
    }
    // Volume matching / user gain may let peaks pass 0 dBFS: turn them down instead of clipping.
    self.eq.crossfade(buf, channels, goal, |wet| self.limiter.process(wet, channels));
    if sidetone {
      // shortcut: during the 20 ms EQ fade the sum only gets the static safety curve.
      add_sidetone(buf, channels, &self.sidetone_buf);
      buf.iter_mut().for_each(|s| *s = soft_limit(*s));
    }
  }
}

/// Next sidetone block (mono, `frames` long) into `block`. True = there is audio to mix in.
fn read_sidetone(
  reader: &mut SidetoneReader,
  block: &mut Vec<f32>,
  layout: &ShmLayout,
  frames: usize,
  rate: f32,
  endpoint: u32,
) -> bool {
  let h = &layout.header;
  let r = Ordering::Relaxed;
  let pair = h.sidetone_output_hash.load(r);
  // Reported only by the instance that is the pair's output or cannot tell: the APO of
  // another output would overwrite it.
  if endpoint == pair || endpoint == 0 {
    h.sidetone_output_hash_seen.store(endpoint, r);
  }
  // Only on the pair's output, and only when this endpoint is known to be it: sidetone in
  // speakers is a feedback loop.
  if !h.sidetone_enabled.load(r) || pair == 0 || pair != endpoint {
    reader.pause();
    return false;
  }
  if block.len() < frames {
    block.resize(frames, 0.0);
  }
  let Some(out) = block.get_mut(..frames) else {
    return false;
  };
  let info = reader.read(&layout.sidetone, out, rate);
  h.sidetone_out_rate.store(rate as u32, r);
  h.sidetone_out_block_frames.store(frames as u32, r);
  h.sidetone_underruns.store(reader.underruns(), r);
  h.sidetone_resyncs.store(reader.resyncs(), r);
  if !info.playing {
    return false;
  }
  h.sidetone_reader_beat.fetch_add(1, r);
  h.sidetone_queued_us.store((u64::from(info.queued) * 1_000_000 / u64::from(info.ring_rate.max(1))) as u32, r);
  // Capture to mix; the units are 100 ns.
  let age_us = now_hns()
    .zip(layout.sidetone.capture_qpc_hns(info.queued))
    .map_or(0, |(now, captured)| now.saturating_sub(captured) / 10);
  h.sidetone_age_us.store(u32::try_from(age_us).unwrap_or(u32::MAX), r);
  true
}

/// Adds the mono sidetone to the first two channels (left, right) of an interleaved buffer.
fn add_sidetone(buf: &mut [f32], channels: usize, mono: &[f32]) {
  for (frame, sample) in buf.chunks_exact_mut(channels.max(1)).zip(mono) {
    frame.iter_mut().take(2).for_each(|channel| *channel += sample);
  }
}
