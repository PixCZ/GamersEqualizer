//! The stream processor: one per app stream, before the mix. Records the stream's level for the
//! pairing, runs its channel's EQ and Automatic ChatMix (`gamersequalizer_common::ducking`).

use super::*;
use gamersequalizer_common::ducking::{DuckRole, DuckSettings, GAP_HNS, StreamDucker, dbg};
use silero_vad::Vad48k;

/// Records the level envelope into its stream-table slot, runs its channel's EQ once the daemon
/// has paired it, then Automatic ChatMix.
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
  ducker: StreamDucker,
  /// Tells the ducking speech from coughs and noise while this stream is on the chat channel
  /// (the daemon pairs it seconds after it starts, so every 48 kHz stream gets one). About 13 KB
  /// per stream: the 1.2 MB of weights are shared per process. None = not 48 kHz, or the weights
  /// failed to load.
  vad: Option<Vad48k>,
  /// The detector's probability for its last completed 32 ms chunk, and when it was last fed.
  vad_prob: f32,
  vad_hns: Option<u64>,
  /// Mono downmix for the detector; grows only for a larger block than seen before.
  mono: Vec<f32>,
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
      let vad = if sample_rate as u32 == 48_000 {
        Vad48k::new()
          .map_err(|e| {
            note(
              Level::Warning,
              format!("Stream: voice detector failed to load ({e}); chat ducking goes by level only"),
            )
          })
          .ok()
      } else {
        None
      };
      StreamProcessor {
        shm,
        slot,
        sample_rate,
        // Room for this stream's channels now: `build` runs on the audio thread.
        eq: EqFade::new(BiquadCascade::with_channels(channels as usize), sample_rate, 4096),
        built: None,
        owner: OwnerWatch::default(),
        ducker: StreamDucker::new(sample_rate),
        mono: Vec::with_capacity(if vad.is_some() { 4096 } else { 0 }),
        vad,
        vad_prob: 0.0,
        vad_hns: None,
        failed: false,
      }
    }))
    .ok()
  }

  /// Records the input level (the daemon pairs on it), then runs the channel EQ and the
  /// ducking in place.
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
    let now = now_hns();
    if let Some(now) = now {
      let peak = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
      slot.record(now, peak, (buf.len() / channels) as u32);
    }
    if self.failed {
      return;
    }
    let bus = StreamBus::from_u32(slot.bus.load(Ordering::Relaxed));
    let ran = catch_unwind(AssertUnwindSafe(|| {
      let active = self.process_eq(buf, channels, bus);
      // The ducking compares streams by block time: without it, this block is left alone.
      if let Some(now) = now {
        self.duck(buf, channels, bus, index, now, active);
      }
    }));
    if ran.is_err() {
      self.failed = true;
      if let Some(slot) =
        self.shm.layout().streams.slots.get(index).filter(|s| s.owner.load(Ordering::Relaxed) == owner)
      {
        dbg::fail(slot);
      }
    }
  }

  /// Returns whether an owner is live and the master switch is on.
  fn process_eq(&mut self, buf: &mut [f32], channels: usize, bus: StreamBus) -> bool {
    let h: &ShmHeader = &self.shm.layout().header;
    let active = self.owner.live(h, Instant::now()) && !h.master_bypass.load(Ordering::Relaxed);
    let target = match bus.eq_index().and_then(|i| h.stream_eq.get(i)) {
      Some(eq) if active => Some((bus, eq.version.load(Ordering::Acquire))),
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
    } else if self.eq.wet >= 1.0 && goal >= 1.0 {
      self.eq.run(buf, channels);
    } else {
      self.eq.crossfade(buf, channels, goal, |_| {});
    }
    active
  }

  /// Automatic ChatMix; without a live owner or with the master switch off it counts as turned
  /// off, and a reduction still applied releases smoothly.
  fn duck(&mut self, buf: &mut [f32], channels: usize, bus: StreamBus, index: usize, now: u64, active: bool) {
    let h = &self.shm.layout().header;
    self.ducker.refresh_tuning(h);
    let mut settings = DuckSettings::read(h);
    settings.enabled &= active;
    let role = DuckRole::from_bus(bus);
    let is_speech = role == DuckRole::Voice && settings.enabled && self.detect_speech(buf, channels, index, now);
    self.ducker.process(buf, channels, role, is_speech, &self.shm.layout().streams, index, now, &settings);
  }

  /// A chat block through the voice detector: the decision of its last completed chunk at the
  /// tuning's `speech_prob`, plus the two telemetry values the log tells "no speech" from "no
  /// detector" by. `duck_sim` (common crate example) mirrors this: keep the two in line.
  fn detect_speech(&mut self, buf: &[f32], channels: usize, index: usize, now: u64) -> bool {
    let (speech, prob, flags) = match self.vad.as_mut() {
      // shortcut: without the detector (stream not at 48 kHz, weights failed to load) any chat
      // sound above the talk threshold counts as speech, coughs and room noise too;
      // upgrade = feed it through a `RateBridge` to 48 kHz.
      None => (true, -1, 0),
      Some(vad) => {
        // The ducker's restart rule. It is not fed while the ducking is off, so turning it on
        // again comes after such a gap too (or within it, from recent audio).
        if self.vad_hns.is_none_or(|t| now.saturating_sub(t) > GAP_HNS) {
          vad.reset();
          self.vad_prob = 0.0;
        }
        self.vad_hns = Some(now);
        let frames = buf.len() / channels.max(1);
        if self.mono.len() < frames {
          self.mono.resize(frames, 0.0);
        }
        if let Some(mono) = self.mono.get_mut(..frames) {
          for (m, frame) in mono.iter_mut().zip(buf.chunks_exact(channels.max(1))) {
            *m = frame.iter().sum::<f32>() / frame.len() as f32;
          }
          let prob = &mut self.vad_prob;
          vad.process(mono, |p| *prob = p);
        }
        let speech = self.vad_prob >= self.ducker.tuning().speech_prob;
        (speech, (self.vad_prob * 1000.0).round() as i32, dbg::FLAG_VAD)
      }
    };
    if let Some(slot) = self.shm.layout().streams.slots.get(index) {
      dbg::set(slot, dbg::VAD_PROB, prob);
      dbg::set(slot, dbg::FLAGS, flags);
    }
    speech
  }

  /// Load the bands of `target`, only when its version changed. Allocates only if the engine
  /// sends another channel count than `create` was told.
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
