//! The echo canceller's speaker reference, written by the output APO of the reference output:
//! the mix after the EQ and before the sidetone (the user's own voice never enters it), folded to
//! stereo, at 48 kHz, with the time it plays, into the ring the mic chain reads
//! (`gamersequalizer-common/src/shm/reference_ring.rs`).
//!
//! The ring has one writer. The daemon names the output (`aec_ref_output_hash`); of the instances
//! on that endpoint (two exist for a moment when Windows re-creates a stream) the one holding
//! `aec_ref_writer_id` writes, and another takes over only after its beat stood still.

use super::*;
use crate::rate_bridge::{PREALLOC_FRAMES, Stage};
use gamersequalizer_common::shm::{ShmRingBuffer, StereoFrame};
use std::time::Duration;

/// Another instance takes the ring over once the writer's beat stood still this long (its
/// stream stopped or was re-created without a clean stop).
const WRITER_TIMEOUT: Duration = Duration::from_millis(300);

pub(super) struct RefFeed {
  /// Device frames per second.
  rate: u32,
  clock: StreamClock,
  /// Device rate -> 48 kHz, stereo; None at 48 kHz.
  stage: Option<Stage>,
  /// The stage's 48 kHz output (interleaved stereo), then as ring frames.
  resampled: Vec<f32>,
  frames: Vec<StereoFrame>,
  writer: WriterClaim,
  /// This instance wrote the previous block.
  writing: bool,
  /// The resampler failed: no reference from this stream any more (the EQ goes on).
  failed: bool,
  /// Blocks written, and whether the last block time was the engine's timestamp (for the note).
  blocks: u64,
  engine_times: bool,
}

/// This instance's claim on `aec_ref_writer_id`.
struct WriterClaim {
  id: u64,
  /// The writer beat last seen, and when it last changed.
  seen: Option<(u32, Instant)>,
}

impl RefFeed {
  /// Everything allocated here, on the configuration thread; the error names the failed resampler.
  pub(super) fn new(rate: u32, instance_id: u32) -> std::result::Result<Self, String> {
    let stage = (rate != REF_SAMPLE_RATE)
      .then(|| Stage::new(rate, REF_SAMPLE_RATE, 2))
      .transpose()
      .map_err(|e| format!("resampler {rate} Hz -> {REF_SAMPLE_RATE} Hz failed ({e})"))?;
    // SAFETY: no arguments, no preconditions.
    let pid = unsafe { GetCurrentProcessId() };
    Ok(Self {
      rate,
      clock: StreamClock::new(rate),
      stage,
      resampled: Vec::with_capacity(2 * PREALLOC_FRAMES),
      frames: Vec::with_capacity(PREALLOC_FRAMES),
      writer: WriterClaim { id: (u64::from(pid) << 32) | u64::from(instance_id), seen: None },
      writing: false,
      failed: false,
      blocks: 0,
      engine_times: false,
    })
  }

  /// Once per block: true when this instance writes the reference now. It does when the owner is
  /// live, the daemon names an output, it is this instance's endpoint, and it holds the ring.
  /// An instance that cannot tell its endpoint (0) bumps `aec_ref_blind_beat` instead. Atomics only.
  pub(super) fn take_part(&mut self, h: &ShmHeader, owner_live: bool, endpoint: u32, now: Instant) -> bool {
    let target = h.aec_ref_output_hash.load(Ordering::Relaxed);
    let wanted = owner_live && target != 0 && !self.failed;
    if wanted && endpoint == 0 {
      h.aec_ref_blind_beat.fetch_add(1, Ordering::Relaxed);
    }
    let writing = wanted && endpoint == target && self.writer.claim(h, now);
    if !writing {
      self.writer.release(h);
    } else if !self.writing {
      // Starting (again): what the stage and the clock hold is from before the pause.
      if let Some(stage) = self.stage.as_mut() {
        stage.reset();
      }
      self.clock = StreamClock::new(self.rate);
    }
    self.writing = writing;
    writing
  }

  /// Writes one block into the ring: `samples` interleaved over `channels`, or `frames` of
  /// silence when None (the engine marked the buffer silent: the speakers play nothing).
  /// `time`: the block's raw time and whether the engine gave it.
  pub(super) fn write(
    &mut self,
    ring: &mut ShmRingBuffer<'_>,
    samples: Option<&[f32]>,
    frames: usize,
    channels: usize,
    time: Option<(u64, bool)>,
  ) {
    if let Some((_, from_engine)) = time {
      self.engine_times = from_engine;
    }
    let Some(t) = self.clock.next(time.map(|(t, _)| t), frames as u64) else {
      return;
    };
    let folded = samples.map(|buf| buf.chunks_exact(channels.max(1)).map(StereoFrame::from_channels));
    self.frames.clear();
    let start = match self.stage.as_mut() {
      None => {
        match folded {
          Some(folded) => self.frames.extend(folded),
          None => self.frames.extend(std::iter::repeat_n(StereoFrame::default(), frames)),
        }
        t
      }
      Some(stage) => {
        // The first 48 kHz frame out is the oldest frame still queued in the stage.
        let start = t.saturating_sub(stage.queued_hns(self.rate));
        match folded {
          Some(folded) => stage.push(folded.flat_map(|f| [f.left, f.right])),
          None => stage.push(std::iter::repeat_n(0.0, 2 * frames)),
        }
        self.resampled.clear();
        if stage.drain_into(&mut self.resampled).is_err() {
          // A buffer mismatch (a bug): stop the reference, keep the EQ; noted when the stream stops.
          self.failed = true;
          return;
        }
        self.frames.extend(self.resampled.as_chunks::<2>().0.iter().map(|&[left, right]| StereoFrame { left, right }));
        start
      }
    };
    if !self.frames.is_empty() {
      ring.push_frames_timed(&self.frames, start);
    }
    ring.header().aec_ref_writer_beat.fetch_add(1, Ordering::Relaxed);
    self.blocks += 1;
  }

  /// Gives the ring up (stream stops): another instance may write at once.
  pub(super) fn release(&self, h: &ShmHeader) {
    self.writer.release(h);
  }

  /// The Application-log note for the end of the stream: whether it fed the reference, and
  /// which time source its block times came from. None = it never was the writer.
  pub(super) fn stop_note(&self) -> Option<(Level, String)> {
    if self.failed {
      let text = "the speaker reference resampler failed: the echo canceller got no reference from this output \
                  until the stream stopped";
      return Some((Level::Warning, text.to_string()));
    }
    let source = if self.engine_times { "the engine's timestamps" } else { "the call time (no engine timestamp)" };
    (self.blocks > 0)
      .then(|| (Level::Info, format!("fed the echo canceller's speaker reference, block times from {source}")))
  }
}

impl WriterClaim {
  /// True while this instance holds the ring; takes it when it is free or its writer went quiet.
  fn claim(&mut self, h: &ShmHeader, now: Instant) -> bool {
    let current = h.aec_ref_writer_id.load(Ordering::Acquire);
    if current == self.id {
      return true;
    }
    let beat = h.aec_ref_writer_beat.load(Ordering::Relaxed);
    let since = match self.seen {
      Some((seen, at)) if seen == beat => at,
      _ => now,
    };
    self.seen = Some((beat, since));
    let free = current == 0 || now.saturating_duration_since(since) >= WRITER_TIMEOUT;
    free && h.aec_ref_writer_id.compare_exchange(current, self.id, Ordering::AcqRel, Ordering::Relaxed).is_ok()
  }

  fn release(&self, h: &ShmHeader) {
    let _ = h.aec_ref_writer_id.compare_exchange(self.id, 0, Ordering::AcqRel, Ordering::Relaxed);
  }
}
