//! The speaker reference: a timed ring of stereo frames the loopback writes and the AEC reads.

use super::*;

/// A stereo sample frame in the ring buffer
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct StereoFrame {
  pub left: f32,
  pub right: f32,
}

impl StereoFrame {
  /// One frame of an interleaved stream. 5.1 and 7.1 fold down the way Windows' own mix does:
  /// the centre and the surrounds join their side 3 dB down, the LFE is left out (a mic hears
  /// little of it, and the chain cuts under 70 Hz).
  /// shortcut: the layout is taken from the channel count (FL FR FC LFE + surround pairs), not
  /// from `dwChannelMask`; other counts keep their first two channels. Only reached when
  /// Windows refuses to convert the loopback to stereo itself.
  pub fn from_channels(frame: &[f32]) -> Self {
    const DOWN_3_DB: f32 = std::f32::consts::FRAC_1_SQRT_2;
    match *frame {
      [] => Self::default(),
      [mono] => Self { left: mono, right: mono },
      [left, right, centre, _lfe, ref surrounds @ ..] if matches!(surrounds.len(), 2 | 4) => {
        let side = |first: usize| surrounds.iter().skip(first).step_by(2).sum::<f32>();
        Self { left: left + (centre + side(0)) * DOWN_3_DB, right: right + (centre + side(1)) * DOWN_3_DB }
      }
      [left, right, ..] => Self { left, right },
    }
  }
}

/// Safe helper for reading and writing frames to the ring buffer
pub struct ShmRingBuffer<'a> {
  layout: &'a mut ShmLayout,
}

impl ShmHeader {
  /// Ring index of the reference frame that played at `qpc_hns`, or None if no
  /// reference was captured yet. May point into the future (reads then yield silence).
  pub fn ref_index_at(&self, qpc_hns: u64) -> Option<u32> {
    for _ in 0..8 {
      let s1 = self.ref_seq.load(Ordering::Acquire);
      if s1 & 1 == 1 {
        std::hint::spin_loop();
        continue;
      }
      let idx = self.ref_snap_index.load(Ordering::Acquire);
      let t = self.ref_snap_qpc_hns.load(Ordering::Acquire);
      let s2 = self.ref_seq.load(Ordering::Acquire);
      if s1 != s2 {
        continue;
      }
      if t == 0 {
        return None;
      }
      let diff_hns = qpc_hns as i64 - t as i64;
      let frames = diff_hns.saturating_mul(REF_SAMPLE_RATE as i64) / HNS_PER_SEC;
      let frames = frames.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
      return Some(idx.wrapping_add_signed(frames));
    }
    None
  }

  /// Age of the newest reference frame relative to `now_hns`, in 100 ns units.
  pub fn ref_age_hns(&self, now_hns: u64) -> Option<u64> {
    let t = self.ref_snap_qpc_hns.load(Ordering::Acquire);
    (t != 0).then(|| now_hns.saturating_sub(t))
  }
}

impl<'a> ShmRingBuffer<'a> {
  pub fn new(layout: &'a mut ShmLayout) -> Self {
    Self { layout }
  }

  /// Write a timestamped loopback packet (48 kHz). `qpc_first_hns` is the QPC time of
  /// `frames[0]`. Gaps (WASAPI loopback delivers nothing while nothing plays) are filled
  /// with silence so ring index stays linear in time.
  pub fn push_frames_timed(&mut self, frames: &[StereoFrame], qpc_first_hns: u64) {
    let header = &self.layout.header;
    let mut write_pos = header.write_frame_index.load(Ordering::Relaxed);
    let snap_t = header.ref_snap_qpc_hns.load(Ordering::Relaxed);

    if snap_t != 0 {
      // Where the writer should be for this packet's timestamp.
      let expected = header.ref_index_at(qpc_first_hns).unwrap_or(write_pos);
      let gap = expected.wrapping_sub(write_pos) as i32;
      if gap > 0 {
        let fill = (gap as usize).min(RING_BUFFER_CAPACITY);
        for _ in 0..fill {
          self.write_slot(write_pos, StereoFrame::default());
          write_pos = write_pos.wrapping_add(1);
        }
        // Huge gap: jump straight to the expected index (ring is all silence anyway).
        write_pos = expected;
      }
    }

    for &frame in frames {
      self.write_slot(write_pos, frame);
      write_pos = write_pos.wrapping_add(1);
    }

    let end_t = qpc_first_hns + (frames.len() as u64 * HNS_PER_SEC as u64) / REF_SAMPLE_RATE as u64;
    let header = &self.layout.header;
    seq_write(&header.ref_seq, || {
      header.write_frame_index.store(write_pos, Ordering::Release);
      header.ref_snap_index.store(write_pos, Ordering::Release);
      header.ref_snap_qpc_hns.store(end_t, Ordering::Release);
    });
  }

  /// Read mono reference frames starting at ring index `start`. Frames that are not in
  /// the ring (overwritten, or not yet written) read as silence.
  pub fn read_frames_from(&self, start: u32, out: &mut [f32]) {
    let write_pos = self.layout.header.write_frame_index.load(Ordering::Acquire);
    let cap = RING_BUFFER_CAPACITY as u32;
    for (i, dst) in out.iter_mut().enumerate() {
      let pos = start.wrapping_add(i as u32);
      let age = write_pos.wrapping_sub(pos); // 1 = newest frame
      *dst = if age >= 1 && age < cap - 1024 { self.read_mono(pos) } else { 0.0 };
    }
    self.layout.header.mic_active.store(true, Ordering::Relaxed);
  }

  /// `read_frames_from` for the reference's two channels.
  pub fn read_stereo_frames_from(&self, start: u32, left: &mut [f32], right: &mut [f32]) {
    let write_pos = self.layout.header.write_frame_index.load(Ordering::Acquire);
    let cap = RING_BUFFER_CAPACITY as u32;
    for (i, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
      let pos = start.wrapping_add(i as u32);
      let age = write_pos.wrapping_sub(pos); // 1 = newest frame
      (*l, *r) = if age >= 1 && age < cap - 1024 { self.read_slot(pos) } else { (0.0, 0.0) };
    }
    self.layout.header.mic_active.store(true, Ordering::Relaxed);
  }

  /// Read speaker reference frames for AEC processing
  pub fn read_recent_frames(&self, count: usize, delay_frames: usize, out: &mut [f32]) {
    let write_pos = self.layout.header.write_frame_index.load(Ordering::Acquire);
    let start = write_pos.wrapping_sub((count + delay_frames) as u32);
    let n = count.min(out.len());
    if let Some(dst) = out.get_mut(..n) {
      for (i, d) in dst.iter_mut().enumerate() {
        *d = self.read_mono(start.wrapping_add(i as u32));
      }
    }
    self.layout.header.mic_active.store(true, Ordering::Relaxed);
  }

  pub fn header(&self) -> &ShmHeader {
    &self.layout.header
  }

  fn write_slot(&mut self, pos: u32, frame: StereoFrame) {
    let idx = pos as usize % RING_BUFFER_CAPACITY;
    if let Some(slot) = self.layout.buffer.get_mut(idx) {
      *slot = frame;
    }
  }

  /// Both channels of a frame; a frame with a non-finite sample is silence.
  fn read_slot(&self, pos: u32) -> (f32, f32) {
    let idx = pos as usize % RING_BUFFER_CAPACITY;
    let frame = self.layout.buffer.get(idx).copied().unwrap_or_default();
    if (frame.left + frame.right).is_finite() {
      (frame.left.clamp(-REF_SAMPLE_MAX, REF_SAMPLE_MAX), frame.right.clamp(-REF_SAMPLE_MAX, REF_SAMPLE_MAX))
    } else {
      (0.0, 0.0)
    }
  }

  /// Mono reference: (left + right) * 0.5.
  fn read_mono(&self, pos: u32) -> f32 {
    let (left, right) = self.read_slot(pos);
    (left + right) * 0.5
  }
}
