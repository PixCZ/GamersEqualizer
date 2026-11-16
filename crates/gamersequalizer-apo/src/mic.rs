//! The mic processor: runs the mic chain on the capture stream and publishes its telemetry.

use super::*;

/// Mic calls per published jitter peak (~1 s at 10 ms blocks).
pub(super) const CALL_TIMING_WINDOW_CALLS: u32 = 100;

/// The mic DSP chain plus its scratch buffers.
/// Mic chains alive in this process: one per microphone in EFX, one per processing mode in
/// use in MFX. The "started" note shows it, so two chains on one mic are visible.
pub(super) static MIC_CHAINS: AtomicU32 = AtomicU32::new(0);

/// Id of the mic chain whose telemetry the app shows (0 = none): the first one to process.
/// Several would overwrite each other's values in the shared memory.
pub(super) static MIC_PUBLISHER: AtomicU32 = AtomicU32::new(0);

/// Bumped by the publisher on every block: the others see it is still being called.
static MIC_PUBLISHER_BEAT: AtomicU32 = AtomicU32::new(0);

/// Source of mic chain ids.
static MIC_CHAIN_IDS: AtomicU32 = AtomicU32::new(0);

/// Blocks (~0.5 s) without the publisher's beat after which another chain takes over: the
/// publisher failed, or its app paused the stream without closing it.
pub(super) const PUBLISHER_QUIET_BLOCKS: u32 = 50;

/// One live mic chain: counted in `MIC_CHAINS`, and maybe the publisher, for as long as it exists.
pub(super) struct MicChainCount {
  pub(super) id: u32,
  /// The publisher's beat this chain saw last, and its own blocks since it last changed.
  seen_beat: u32,
  quiet_blocks: u32,
}

pub(super) struct MicProcessor {
  chain: Box<MicChain>,
  pub(super) sidetone: Box<SidetoneTap>,
  mono_in: Vec<f32>,
  mono_out: Vec<f32>,
  /// Resampling to/from 48 kHz when the mic runs at another rate; None at 48 kHz.
  pub(super) bridge: Option<RateBridge>,
  /// Set after a panic in the chain: pass audio through from then on.
  pub(super) failed: bool,
  clock: MicClock,
  owner: OwnerWatch,
  /// Pass through unless a daemon owns the mapping. Only tests turn it off (no daemon there).
  pub(super) follow_owner: bool,
  /// The chain ran on the previous block. After a pass-through stretch the bridge FIFOs hold
  /// audio from before it.
  was_live: bool,
  timing: CallTiming,
  /// Blocks the engine handed over / the chain processed, for the note when the stream stops.
  blocks: u64,
  processed: u64,
  count: MicChainCount,
  endpoint_tag: Option<String>,
  instance_id: u32,
}

/// How regularly the audio engine calls the mic APO (debug log): an app gets its data late
/// when a call comes late, e.g. because the previous one blocked.
#[derive(Default)]
pub(super) struct CallTiming {
  last: Option<Instant>,
  calls: u32,
  jitter_peak_us: u64,
}

// SAFETY: the chain holds `Rc`s internally (DeepFilterNet's tract graph) that never leave
// it; the whole Processor moves between the configuration and real-time threads as one
// unit behind a Mutex, so no Rc is ever touched from two threads at once.
unsafe impl Send for MicProcessor {}

impl MicChainCount {
  pub(super) fn new() -> Self {
    MIC_CHAINS.fetch_add(1, Ordering::Relaxed);
    // Never 0 ("no publisher"), also after the counter wraps.
    let id = MIC_CHAIN_IDS.fetch_add(1, Ordering::Relaxed).wrapping_add(1).max(1);
    Self { id, seen_beat: 0, quiet_blocks: 0 }
  }

  /// One block of this chain: true when it is the one that publishes telemetry. It takes
  /// over when there is no publisher (the first chain; the previous one's stream closed) or
  /// the publisher went quiet. Atomics only: called on the real-time thread.
  pub(super) fn publishes(&mut self) -> bool {
    let current = MIC_PUBLISHER.load(Ordering::Relaxed);
    if current == self.id {
      MIC_PUBLISHER_BEAT.fetch_add(1, Ordering::Relaxed);
      return true;
    }
    if current != 0 {
      let beat = MIC_PUBLISHER_BEAT.load(Ordering::Relaxed);
      if beat != self.seen_beat {
        self.seen_beat = beat;
        self.quiet_blocks = 0;
        return false;
      }
      self.quiet_blocks += 1;
      if self.quiet_blocks < PUBLISHER_QUIET_BLOCKS {
        return false;
      }
    }
    self.quiet_blocks = 0;
    MIC_PUBLISHER.compare_exchange(current, self.id, Ordering::AcqRel, Ordering::Relaxed).is_ok()
  }
}

impl Drop for MicChainCount {
  fn drop(&mut self) {
    let _ = MIC_PUBLISHER.compare_exchange(self.id, 0, Ordering::AcqRel, Ordering::Relaxed);
    MIC_CHAINS.fetch_sub(1, Ordering::Relaxed);
  }
}

impl MicProcessor {
  /// Opens/creates the Global mapping and builds the chain for a `rate` Hz stream (resampled
  /// to 48 kHz unless it already is). None (pass-through) on an unsupported rate or failure.
  pub(super) fn create(rate: u32, endpoint_tag: Option<String>, instance_id: u32) -> Option<Box<Self>> {
    let refuse = |why: String| {
      note_tagged(
        ApoKind::Mic,
        endpoint_tag.as_deref(),
        instance_id,
        Level::Warning,
        format!("{why}; passes audio through"),
      );
    };
    let bridge = match RateBridge::for_rate(rate) {
      Some(Ok(bridge)) => Some(bridge),
      Some(Err(e)) => {
        refuse(format!("resampler for {rate} Hz failed ({e})"));
        return None;
      }
      None if rate == REF_SAMPLE_RATE => None,
      None => {
        refuse(format!("{rate} Hz is not supported"));
        return None;
      }
    };
    let shm_tag = endpoint_tag.clone();
    let built = catch_unwind(AssertUnwindSafe(move || {
      let shm = shared_memory_tagged(ApoKind::Mic, shm_tag.as_deref(), instance_id);
      let mut chain = Box::new(MicChain::with_shm(shm));
      chain.preload_selected_model();
      #[cfg(feature = "debug-capture")]
      if let Ok(map) = gamersequalizer_mic_chain::debug_capture::DebugCaptureMap::create(APO_SHM_SDDL) {
        chain.attach_debug_capture(map, rate);
      }
      Box::new(MicProcessor {
        chain,
        sidetone: Box::new(SidetoneTap::new(rate, instance_id)),
        mono_in: vec![0.0; 4096],
        mono_out: vec![0.0; 4096],
        bridge,
        failed: false,
        clock: MicClock::new(rate),
        owner: OwnerWatch::default(),
        follow_owner: true,
        was_live: false,
        timing: CallTiming::default(),
        blocks: 0,
        processed: 0,
        count: MicChainCount::new(),
        endpoint_tag: shm_tag,
        instance_id,
      })
    }));
    if built.is_err() {
      refuse("building the chain panicked".to_string());
    }
    built.ok()
  }

  /// Application-log note when the stream stops: did the chain really run, and if not, why.
  /// Keyed per endpoint + reason, so each outcome is written once per endpoint per audiodg run.
  pub(super) fn note_stop(&self) {
    let why = if self.failed {
      "the chain failed and passed audio through from then on"
    } else if self.blocks == 0 {
      "no audio blocks arrived"
    } else if self.processed > 0 {
      "processed"
    } else if self.chain.shm_header().is_none() {
      "no shared memory, passed audio through"
    } else {
      "GamersEqualizer did not drive it (not running for the user at the console), passed audio through"
    };
    let level = if self.processed > 0 { Level::Info } else { Level::Warning };
    let label = match &self.endpoint_tag {
      Some(tag) => format!("Mic chain [{tag}]"),
      None => "Mic chain".to_string(),
    };
    let text = format!("{label}: stream stopped after {} blocks, {} processed: {why}", self.blocks, self.processed);
    let id = self.endpoint_tag.clone().unwrap_or_else(|| format!("#{}", self.instance_id));
    event_log::note(level, &format!("mic stop:{id}:{why}"), &text);
    if self.sidetone.failed {
      let text = format!("{label}: the sidetone tap failed and stayed silent until this stream stopped");
      event_log::note(Level::Warning, &format!("sidetone failed:{id}"), &text);
    }
  }

  /// In place on interleaved `buf`: first channel through the chain, result to all channels.
  pub(super) fn process(&mut self, buf: &mut [f32], channels: usize, qpc_hns: Option<u64>) {
    self.blocks += 1;
    if channels == 0 {
      return;
    }
    let now = Instant::now();
    // One look per block, for the chain and its sidetone tap.
    let owner_live = self.chain.shm_header().is_some_and(|h| self.owner.live(h, now));
    // Sidetone first, from the untouched input; it has its own failure flag.
    if let Some(layout) = self.chain.shm_layout() {
      // This chain's own leveler, not the shared memory: every mic APO instance publishes there.
      // A failed chain no longer levels, so the sidetone goes back to the raw microphone.
      let leveler_gain_db = if self.failed { 0.0 } else { self.chain.leveler_gain_db() };
      self.sidetone.process(layout, buf, channels, qpc_hns, leveler_gain_db, owner_live);
    }
    if self.failed {
      return;
    }
    let frames = buf.len() / channels;
    if self.mono_in.len() < frames {
      // Only if the engine exceeds the preallocated 85 ms period.
      self.mono_in.resize(frames, 0.0);
      self.mono_out.resize(frames, 0.0);
    }
    let qpc_hns = self.clock.next(qpc_hns, frames as u64);
    // With several mic chains (MFX: one per processing mode in use) the app shows one of them.
    let publishes = self.count.publishes();
    self.chain.set_publishing(publishes, MIC_CHAINS.load(Ordering::Relaxed) > 1);
    self.timing.note(now, frames, self.clock.rate, self.chain.shm_header().filter(|_| publishes));
    // Nobody's settings to apply (app not running for the user at the console): the raw mic.
    let Self { chain, mono_in, mono_out, bridge, failed, follow_owner, was_live, processed, .. } = self;
    if *follow_owner && !owner_live {
      if std::mem::replace(was_live, false) {
        chain.pause();
      }
      return;
    }
    if !std::mem::replace(was_live, true)
      && let Some(bridge) = bridge.as_mut()
    {
      bridge.reset();
    }
    let (Some(input), Some(output)) = (mono_in.get_mut(..frames), mono_out.get_mut(..frames)) else {
      return;
    };
    for (dst, frame) in input.iter_mut().zip(buf.chunks_exact(channels)) {
      *dst = frame.first().copied().unwrap_or(0.0);
    }
    let ran = catch_unwind(AssertUnwindSafe(|| match bridge {
      // Other rate: the bridge resamples to 48 kHz, runs the chain, and writes back into `input`.
      Some(bridge) => {
        let resampled = bridge.process(input, qpc_hns, |chain_in, chain_out, time| {
          chain.process_timed(&[chain_in], &mut [chain_out], time);
        });
        // The sinc filter rings slightly past the chain's limiter ceiling: keep it at full scale.
        for (dst, &src) in output.iter_mut().zip(input.iter()) {
          *dst = src.clamp(-1.0, 1.0);
        }
        resampled.is_ok()
      }
      None => {
        chain.process_timed(&[&*input], &mut [&mut *output], qpc_hns);
        true
      }
    }));
    if !matches!(ran, Ok(true)) {
      // Panic in the chain, or a resampler buffer mismatch (a bug): pass through from now on.
      *failed = true;
      return;
    }
    *processed += 1;
    for (frame, &sample) in buf.chunks_exact_mut(channels).zip(output.iter()) {
      frame.fill(sample);
    }
  }
}

impl Drop for MicProcessor {
  fn drop(&mut self) {
    if let Some(layout) = self.chain.shm_layout() {
      self.sidetone.release(layout);
    }
  }
}

impl CallTiming {
  /// One engine call at `now` for a block of `frames` at `rate` Hz.
  pub(super) fn note(&mut self, now: Instant, frames: usize, rate: u32, h: Option<&ShmHeader>) {
    let block_us = frames as u64 * 1_000_000 / u64::from(rate.max(1));
    if let Some(last) = self.last.replace(now) {
      let interval_us = u64::try_from(now.duration_since(last).as_micros()).unwrap_or(u64::MAX);
      self.jitter_peak_us = self.jitter_peak_us.max(interval_us.abs_diff(block_us));
      if interval_us.saturating_mul(2) > block_us.saturating_mul(3)
        && let Some(h) = h
      {
        h.mic_call_gaps.fetch_add(1, Ordering::Relaxed);
      }
    }
    self.calls += 1;
    if self.calls >= CALL_TIMING_WINDOW_CALLS {
      if let Some(h) = h {
        h.mic_call_jitter_peak_us.store(u32::try_from(self.jitter_peak_us).unwrap_or(u32::MAX), Ordering::Relaxed);
      }
      self.calls = 0;
      self.jitter_peak_us = 0;
    }
  }
}
