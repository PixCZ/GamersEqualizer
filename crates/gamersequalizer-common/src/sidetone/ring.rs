//! The sample ring in the Global mapping (mic APO writes) and the reader in the output APO.

use super::*;

/// Samples in the ring: 341 ms at 48 kHz, 85 ms at 192 kHz.
pub const SIDETONE_RING_LEN: usize = 16_384;
const RING_MASK: u32 = SIDETONE_RING_LEN as u32 - 1;
const _: () = assert!(SIDETONE_RING_LEN.is_power_of_two());
/// Refused claims in a row without a write in between before an owner of the same process
/// counts as gone (one claim per block of the claimer: 85 ms at the shortest period).
pub(super) const STALE_CLAIMS: u32 = 32;
/// The reader stays this far from where the writer writes next.
const RING_GUARD: usize = 256;
const HNS_PER_SEC: f64 = 10_000_000.0;

/// Reader: queued audio kept beyond what one call needs, against late calls of either APO.
pub(super) const MARGIN_SECONDS: f32 = 0.002;
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
const STALE_SECONDS: f32 = 0.100;
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
  /// Write index at the last refused claim, and how many refused claims in a row saw it
  /// unchanged: an owner that stopped writing without releasing is replaced.
  stale_seen: AtomicU32,
  stale_count: AtomicU32,
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

impl Default for SidetoneRing {
  fn default() -> Self {
    Self {
      write_index: AtomicU32::new(0),
      rate: AtomicU32::new(0),
      write_qpc_hns: AtomicU64::new(0),
      owner: AtomicU64::new(0),
      stale_seen: AtomicU32::new(0),
      stale_count: AtomicU32::new(0),
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
    let mut free = current == 0 || (current >> 32) as u32 != pid;
    if !free {
      // An owner in this process: alive if it wrote since our last try (a writer writes
      // every block or releases). One whose stream stopped without a release never moves.
      let head = self.write_index.load(Ordering::Relaxed);
      if self.stale_seen.swap(head, Ordering::Relaxed) == head {
        free = self.stale_count.fetch_add(1, Ordering::Relaxed) + 1 >= STALE_CLAIMS;
      } else {
        self.stale_count.store(0, Ordering::Relaxed);
      }
    }
    if free && self.owner.compare_exchange(current, owner, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
      self.stale_count.store(0, Ordering::Relaxed);
      Some(owner)
    } else {
      None
    }
  }

  /// Gives the ring up. The rate goes to 0, which tells the readers "stopped on purpose":
  /// they go quiet without counting it as running dry.
  pub fn release_writer(&self, owner: u64) {
    if self.owner.compare_exchange(owner, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
      self.rate.store(0, Ordering::Relaxed);
    }
  }

  /// Appends a block. `end_qpc_hns`: capture time of the sample after the last one, 0 if
  /// unknown. False (and nothing written) unless `owner` holds the ring: the caller lost it
  /// and has to claim again.
  pub fn write(&self, owner: u64, samples: &[f32], rate: u32, end_qpc_hns: u64) -> bool {
    if self.owner.load(Ordering::Acquire) != owner || owner == 0 {
      return false;
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
    true
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

  pub(super) fn sample(&self, index: u32) -> f32 {
    let value =
      self.samples.get((index & RING_MASK) as usize).map_or(0.0, |s| f32::from_bits(s.load(Ordering::Relaxed)));
    if value.is_finite() && value.abs() <= SAMPLE_MAX { value } else { 0.0 }
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
    let seconds = |s: f32| i64::from((s * ring_rate) as u32);
    let stale =
      (need + margin + seconds(START_EXTRA_SECONDS + STALE_SECONDS)).min((SIDETONE_RING_LEN - RING_GUARD) as i64);
    if need + margin > stale {
      // One call needs more than the ring holds (a huge output block): cannot work.
      self.state = ReaderState::Idle;
      return silent(0);
    }
    if avail > stale {
      // The reader was not called for a while (the output stood still): what piled up is old,
      // start over at the write head.
      self.state = ReaderState::Idle;
      return silent(0);
    }
    if self.state == ReaderState::Priming {
      if avail < (need + margin + seconds(START_EXTRA_SECONDS)).min(stale) {
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
