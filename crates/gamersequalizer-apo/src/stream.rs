//! The stream EQ processor: one per app stream, before the mix.

use super::*;

/// Stream EQ: records the level envelope into its stream-table slot and runs its channel's
/// EQ once the daemon has paired it.
pub(super) struct StreamProcessor {
  shm: SharedMemoryClient,
  /// (slot index, owner value); None = table full, run unregistered (pass-through).
  pub(super) slot: Option<(usize, u64)>,
  sample_rate: f32,
  /// Shared coefficients with per-channel filter state, for the bus/version in `built`.
  cascade: BiquadCascade,
  preamp_gain: f32,
  /// (bus, stream EQ version) the cascade holds; None = nothing built (pass-through).
  built: Option<(StreamBus, u32)>,
  /// Share of the processed signal in the output, 0..1, moved by `fade_step` per frame.
  pub(super) wet: f32,
  fade_step: f32,
  /// Processed copy while crossfading. Grows (allocates) only if the engine sends a
  /// larger buffer than before.
  scratch: Vec<f32>,
  /// No live owner (see `OwnerWatch`): treated like the master switch, the EQ fades out.
  owner: OwnerWatch,
  pub(super) failed: bool,
}

impl StreamProcessor {
  /// Claims a stream-table slot. None only if building panicked (pass-through).
  pub(super) fn create(shm: SharedMemoryClient, sample_rate: f32, channels: u32, mode: u128) -> Option<Self> {
    catch_unwind(AssertUnwindSafe(move || {
      // SAFETY: no preconditions.
      let pid = unsafe { GetCurrentProcessId() };
      let info = StreamInfo { pid, now_hns: now_hns().unwrap_or(0), sample_rate: sample_rate as u32, channels, mode };
      let slot = shm.layout().streams.claim(&info);
      StreamProcessor {
        shm,
        slot,
        sample_rate,
        cascade: BiquadCascade::default(),
        preamp_gain: 1.0,
        built: None,
        wet: 0.0,
        fade_step: 1.0 / (STREAM_FADE_SECONDS * sample_rate.max(1.0)),
        scratch: Vec::with_capacity(4096),
        owner: OwnerWatch::default(),
        failed: false,
      }
    }))
    .ok()
  }

  /// Records the input level (the daemon pairs on it), then runs the channel EQ in place.
  pub(super) fn process(&mut self, buf: &mut [f32], channels: usize) {
    if channels == 0 {
      return;
    }
    let Some((index, _)) = self.slot else {
      return;
    };
    let Some(slot) = self.shm.layout().streams.slots.get(index) else {
      return;
    };
    if let Some(now) = now_hns() {
      let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
      slot.record(now, peak, (buf.len() / channels) as u32);
    }
    if self.failed {
      return;
    }
    let bus = StreamBus::from_u32(slot.bus.load(Ordering::Relaxed));
    if catch_unwind(AssertUnwindSafe(|| self.process_eq(buf, channels, bus))).is_err() {
      self.failed = true;
    }
  }

  fn process_eq(&mut self, buf: &mut [f32], channels: usize, bus: StreamBus) {
    let h: &ShmHeader = &self.shm.layout().header;
    let owned = self.owner.live(h, Instant::now());
    let target = match bus.eq_index().and_then(|i| h.stream_eq.get(i)) {
      Some(eq) if owned && !h.master_bypass.load(Ordering::Relaxed) => Some((bus, eq.version.load(Ordering::Acquire))),
      _ => None,
    };
    match (self.built, target) {
      (built, target) if built == target => {}
      // Same channel, edited EQ: new coefficients, filter state kept.
      (Some((old_bus, _)), Some((new_bus, _))) if old_bus == new_bus => self.build(target, channels, false),
      // Channel change: fade the old EQ out first, then start the new one from silence.
      _ if self.wet <= 0.0 => self.build(target, channels, true),
      _ => {}
    }
    let goal = if self.built.is_some() && self.built == target { 1.0 } else { 0.0 };
    if self.built.is_none() || (self.wet <= 0.0 && goal <= 0.0) {
      self.wet = 0.0;
      return;
    }
    if self.wet >= 1.0 && goal >= 1.0 {
      self.run_eq_in_place(buf, channels);
      return;
    }
    // Crossfade: dry + wet * (processed - dry), wet ramping per frame.
    if self.scratch.len() < buf.len() {
      self.scratch.resize(buf.len(), 0.0);
    }
    let mut scratch = std::mem::take(&mut self.scratch);
    if let Some(wet_buf) = scratch.get_mut(..buf.len()) {
      wet_buf.copy_from_slice(buf);
      self.run_eq_in_place(wet_buf, channels);
      for (dry_frame, wet_frame) in buf.chunks_exact_mut(channels).zip(wet_buf.chunks_exact(channels)) {
        self.wet =
          if goal > self.wet { (self.wet + self.fade_step).min(1.0) } else { (self.wet - self.fade_step).max(0.0) };
        for (d, w) in dry_frame.iter_mut().zip(wet_frame) {
          *d += self.wet * (w - *d);
        }
      }
    }
    self.scratch = scratch;
  }

  /// Load the bands of `target` (allocates; only when its version changed).
  fn build(&mut self, target: Option<(StreamBus, u32)>, channels: usize, fresh: bool) {
    self.built = target;
    let Some(eq) = target.and_then(|(bus, _)| bus.eq_index()).and_then(|i| self.shm.layout().header.stream_eq.get(i))
    else {
      self.cascade.clear();
      self.preamp_gain = 1.0;
      return;
    };
    let (bands, preamp_db) = eq.read();
    if fresh {
      self.cascade.reset_state();
    }
    self.cascade.set_channels(channels);
    self.cascade.set_bands(bands, self.sample_rate);
    self.preamp_gain = 10f32.powf(preamp_db / 20.0);
  }

  fn run_eq_in_place(&mut self, buf: &mut [f32], channels: usize) {
    if self.preamp_gain != 1.0 {
      buf.iter_mut().for_each(|s| *s *= self.preamp_gain);
    }
    self.cascade.process_interleaved(buf, channels);
  }
}

impl Drop for StreamProcessor {
  fn drop(&mut self) {
    if let Some((index, owner)) = self.slot {
      self.shm.layout().streams.release(index, owner);
    }
  }
}
