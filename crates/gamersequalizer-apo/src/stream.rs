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
  pub(super) eq: EqFade,
  /// (bus, stream EQ version) the cascade holds; None = nothing built (pass-through).
  built: Option<(StreamBus, u32)>,
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
        eq: EqFade::new(BiquadCascade::default(), sample_rate, 4096),
        built: None,
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
    let Some((index, owner)) = self.slot else {
      return;
    };
    // Another instance took the slot over (it counted this one stale): its level and channel
    // are not this stream's, so pass through.
    let Some(slot) = self.shm.layout().streams.slots.get(index).filter(|s| s.owner.load(Ordering::Relaxed) == owner)
    else {
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
      _ if self.eq.wet <= 0.0 => self.build(target, channels, true),
      _ => {}
    }
    let goal = if self.built.is_some() && self.built == target { 1.0 } else { 0.0 };
    if self.built.is_none() || (self.eq.wet <= 0.0 && goal <= 0.0) {
      self.eq.wet = 0.0;
      return;
    }
    if self.eq.wet >= 1.0 && goal >= 1.0 {
      self.eq.run(buf, channels);
      return;
    }
    self.eq.crossfade(buf, channels, goal, |_| {});
  }

  /// Load the bands of `target` (allocates; only when its version changed).
  fn build(&mut self, target: Option<(StreamBus, u32)>, channels: usize, fresh: bool) {
    self.built = target;
    let Some(eq) = target.and_then(|(bus, _)| bus.eq_index()).and_then(|i| self.shm.layout().header.stream_eq.get(i))
    else {
      self.eq.cascade.clear();
      self.eq.preamp_gain = 1.0;
      return;
    };
    let (bands, preamp_db) = eq.read();
    if fresh {
      self.eq.cascade.reset_state();
    }
    self.eq.cascade.set_channels(channels);
    self.eq.cascade.set_bands(bands, self.sample_rate);
    self.eq.set_preamp_db(preamp_db);
  }
}

impl Drop for StreamProcessor {
  fn drop(&mut self) {
    if let Some((index, owner)) = self.slot {
      self.shm.layout().streams.release(index, owner);
    }
  }
}
