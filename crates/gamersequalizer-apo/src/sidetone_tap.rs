//! The sidetone tap of the mic APO: raw input -> EQ -> noise suppression -> level -> ring.

use super::*;

/// Sidetone telemetry (peak, gate, mode) is published every this many blocks.
const SIDETONE_TELEMETRY_BLOCKS: u32 = 10;

/// Sidetone tap of the mic APO: raw input -> EQ -> noise suppression -> level -> ring.
/// Independent of the mic chain: it runs when the chain failed and only reads the buffer.
pub(super) struct SidetoneTap {
  chain: Box<SidetoneChain>,
  /// `sidetone_eq_version` the chain's EQ was built from.
  eq_version: Option<u32>,
  /// Frame-based suppressors (48 kHz only); None at another rate: the expander stands in.
  denoiser: Option<Box<SidetoneDenoiser>>,
  rate: u32,
  /// `device_hash` of the endpoint this APO sits on; 0 = unknown.
  pub(super) endpoint_hash: u32,
  instance: u32,
  /// Owner value while this tap writes the ring.
  owner: Option<u64>,
  mono: Vec<f32>,
  peak: f32,
  blocks: u32,
  pub(super) failed: bool,
}

impl SidetoneTap {
  /// `instance`: the APO's `instance_id`, unique in this process (ring owner = process id + it).
  pub(super) fn new(rate: u32, instance: u32) -> Self {
    Self {
      chain: Box::new(SidetoneChain::new(rate as f32)),
      eq_version: None,
      denoiser: (rate == sidetone_denoise::SAMPLE_RATE).then(|| Box::new(SidetoneDenoiser::new())),
      rate,
      endpoint_hash: 0,
      instance,
      owner: None,
      // ~85 ms at 48 kHz; grows only if the engine sends longer blocks.
      mono: vec![0.0; 4096],
      peak: 0.0,
      blocks: 0,
      failed: false,
    }
  }

  /// `buf`: the interleaved raw input of this block. Never changed.
  /// `owner_live`: the mic APO's `OwnerWatch` for this block.
  pub(super) fn process(
    &mut self,
    layout: &ShmLayout,
    buf: &[f32],
    channels: usize,
    qpc_hns: Option<u64>,
    leveler_gain_db: f32,
    owner_live: bool,
  ) {
    if self.failed || channels == 0 {
      return;
    }
    let inner = || self.process_inner(layout, buf, channels, qpc_hns, leveler_gain_db, owner_live);
    if catch_unwind(AssertUnwindSafe(inner)).is_err() {
      self.failed = true;
      self.release(layout);
    }
  }

  fn process_inner(
    &mut self,
    layout: &ShmLayout,
    buf: &[f32],
    channels: usize,
    qpc_hns: Option<u64>,
    leveler_gain_db: f32,
    owner_live: bool,
  ) {
    let h = &layout.header;
    let r = Ordering::Relaxed;
    let pair_hash = h.sidetone_mic_hash.load(r);
    // Reported only by the instance that is the pair's microphone or cannot tell: the APO of
    // another microphone would overwrite it.
    if self.endpoint_hash == pair_hash || self.endpoint_hash == 0 {
      h.sidetone_mic_hash_seen.store(self.endpoint_hash, r);
    }
    // The pair's microphone. An endpoint we could not identify may write too: a wrong
    // microphone is only a wrong voice, and the ring takes one writer.
    let ours = pair_hash != 0 && (self.endpoint_hash == pair_hash || self.endpoint_hash == 0);
    let mut settings = h.read_sidetone();
    settings.enabled &= ours && owner_live && !h.master_bypass.load(r);
    self.chain.configure(&settings, leveler_gain_db, h.mic_muted.load(r));
    let eq_version = h.sidetone_eq_version.load(Ordering::Acquire);
    if self.eq_version != Some(eq_version) {
      self.eq_version = Some(eq_version);
      self.chain.set_eq(h.read_sidetone_eq());
    }
    if self.chain.is_silent() {
      // Off, muted or not our pair: give the ring up (a clean stop for the reader), and tell
      // the suppressors about the pause, so nothing from before it is played afterwards.
      self.release(layout);
      if let Some(denoiser) = self.denoiser.as_mut() {
        denoiser.process(NoiseMode::Off, 0.0, &mut []);
      }
      return;
    }
    if self.owner.is_none() {
      // SAFETY: no preconditions.
      self.owner = layout.sidetone.claim_writer(unsafe { GetCurrentProcessId() }, self.instance);
      if self.owner.is_some() {
        // Filter memory and levels are from before the pause; the level fades in from zero.
        self.chain.reset();
      }
    }
    let Some(owner) = self.owner else {
      return; // another mic APO instance holds the ring
    };

    let frames = buf.len() / channels;
    if self.mono.len() < frames {
      self.mono.resize(frames, 0.0);
    }
    let Some(mono) = self.mono.get_mut(..frames) else {
      return;
    };
    for (dst, frame) in mono.iter_mut().zip(buf.chunks_exact(channels)) {
      *dst = frame.first().copied().unwrap_or(0.0);
    }
    self.chain.run_eq(mono);
    let wanted = self.chain.noise_mode();
    let mut active = if wanted == NoiseMode::Off { NoiseMode::Off } else { NoiseMode::Expander };
    if let Some(denoiser) = self.denoiser.as_mut() {
      if wanted.is_framed() {
        active = denoiser.process(wanted, settings.gate_range_db, mono);
      } else {
        // Tells it about the pause, so the next frame-based run starts fresh.
        denoiser.process(wanted, 0.0, &mut []);
      }
    }
    if active == NoiseMode::Expander {
      self.chain.expander_mut().process(mono);
    }
    self.chain.run_level(mono);
    let end_qpc = qpc_hns.map_or(0, |t| t.saturating_add(frames_to_hns(frames as u64, self.rate).max(0) as u64));
    if !layout.sidetone.write(owner, mono, self.rate, end_qpc) {
      // Another instance took the ring over (we were not called for a while): claim again.
      self.owner = None;
      return;
    }

    h.sidetone_writer_beat.fetch_add(1, r);
    h.sidetone_mic_rate.store(self.rate, r);
    h.sidetone_mic_block_frames.store(frames as u32, r);
    self.peak = mono.iter().fold(self.peak, |p, s| p.max(s.abs()));
    self.blocks += 1;
    if self.blocks >= SIDETONE_TELEMETRY_BLOCKS {
      h.sidetone_peak_permille.store((self.peak * 1000.0) as u32, r);
      let gate_db = if active == NoiseMode::Expander { self.chain.expander_mut().gain_db() } else { 0.0 };
      h.sidetone_gate_gain_db_x10.store((gate_db * 10.0) as i32, r);
      h.sidetone_noise_mode_active.store(active.to_u32(), r);
      let delay = self.denoiser.as_ref().map_or(0, |d| d.delay_samples(active)) as u64;
      h.sidetone_noise_delay_us.store((delay * 1_000_000 / u64::from(sidetone_denoise::SAMPLE_RATE)) as u32, r);
      (self.peak, self.blocks) = (0.0, 0);
    }
  }

  pub(super) fn release(&mut self, layout: &ShmLayout) {
    if let Some(owner) = self.owner.take() {
      layout.sidetone.release_writer(owner);
    }
  }
}
