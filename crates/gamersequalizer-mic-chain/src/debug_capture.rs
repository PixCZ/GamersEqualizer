//! Debug capture mapping (layout and protocol in `gamersequalizer_common::debug_capture`).
//! `DebugCaptureMap` is always built (the capture tool reads through it); `CaptureWriter`,
//! which the mic chain feeds, only with the `debug-capture` feature.

use crate::shared_memory::{create_mapping, wide};
use gamersequalizer_common::debug_capture::*;
use std::ptr::{NonNull, addr_of};
use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
  FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
};
use windows::core::PCWSTR;

pub struct DebugCaptureMap {
  handle: HANDLE,
  ptr: NonNull<DebugCaptureLayout>,
}

// SAFETY: process-wide memory; the header is atomics, audio/vars are only written by the one
// stream that owns the capture and read by the tool after `STATE_DONE` (Release/Acquire).
unsafe impl Send for DebugCaptureMap {}

impl DebugCaptureMap {
  /// Create (or open) the Global mapping; `sddl` must let the logged-on user open it.
  pub fn create(sddl: &str) -> Result<Self, String> {
    Self::create_named(DEBUG_CAPTURE_NAME, Some(sddl))
  }

  fn create_named(name: &str, sddl: Option<&str>) -> Result<Self, String> {
    let handle = create_mapping(name, sddl, DebugCaptureLayout::total_size_bytes())?;
    // SAFETY: a mapping of the layout's size.
    let map = unsafe { Self::map(handle)? };
    let h = map.header();
    if h.magic.load(Ordering::Acquire) != DEBUG_CAPTURE_MAGIC {
      h.state.store(STATE_IDLE, Ordering::Relaxed);
      h.magic.store(DEBUG_CAPTURE_MAGIC, Ordering::Release);
    }
    Ok(map)
  }

  /// Open the mapping a debug-capture APO created (fails when no such APO is streaming).
  pub fn open_existing() -> Result<Self, String> {
    let name = wide(DEBUG_CAPTURE_NAME);
    // SAFETY: NUL-terminated name owned by this frame; the mapping has the layout's size.
    unsafe {
      let handle = OpenFileMappingW((FILE_MAP_READ | FILE_MAP_WRITE).0, false, PCWSTR(name.as_ptr()))
        .map_err(|e| format!("OpenFileMappingW({DEBUG_CAPTURE_NAME}) failed: {e}"))?;
      let map = Self::map(handle)?;
      if map.header().magic.load(Ordering::Acquire) != DEBUG_CAPTURE_MAGIC {
        return Err("debug capture mapping is not initialized".to_string());
      }
      Ok(map)
    }
  }

  pub fn header(&self) -> &DebugCaptureHeader {
    // SAFETY: the view outlives `self`; the header is atomics only.
    unsafe { &*addr_of!((*self.ptr.as_ptr()).header) }
  }

  /// Recorded audio of `stage`, `frames` long. Read only after `STATE_DONE`.
  pub fn stage(&self, stage: usize, frames: usize) -> &[f32] {
    if stage >= DEBUG_STAGES.len() {
      return &[];
    }
    // SAFETY: in bounds (checked above, frames clamped); no writer after `STATE_DONE`.
    unsafe {
      #[expect(clippy::indexing_slicing, reason = "stage < DEBUG_STAGES.len(), checked above")]
      let base = addr_of!((*self.ptr.as_ptr()).audio[stage]) as *const f32;
      std::slice::from_raw_parts(base, frames.min(DEBUG_CAPTURE_MAX_FRAMES))
    }
  }

  /// Variable rows, `blocks` long. Read only after `STATE_DONE`.
  pub fn vars(&self, blocks: usize) -> &[[f32; DEBUG_VARS.len()]] {
    // SAFETY: as `stage`.
    unsafe {
      let base = addr_of!((*self.ptr.as_ptr()).vars) as *const [f32; DEBUG_VARS.len()];
      std::slice::from_raw_parts(base, blocks.min(DEBUG_CAPTURE_MAX_BLOCKS))
    }
  }

  /// Takes ownership of `handle` (closed on failure).
  ///
  /// # Safety
  /// `handle` is a file mapping handle the caller owns and does not close itself.
  unsafe fn map(handle: HANDLE) -> Result<Self, String> {
    // SAFETY: the caller hands the handle over (contract above). The view is the layout's size or
    // the call fails; it and the handle are released once, in `Drop`, or the handle here on failure.
    unsafe {
      let view = MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, DebugCaptureLayout::total_size_bytes());
      let Some(ptr) = NonNull::new(view.Value as *mut DebugCaptureLayout) else {
        let _ = CloseHandle(handle);
        return Err("MapViewOfFile returned null pointer".to_string());
      };
      Ok(Self { handle, ptr })
    }
  }

  #[cfg(feature = "debug-capture")]
  fn write_audio(&self, stage: usize, at: usize, samples: &[f32]) {
    let n = samples.len().min(DEBUG_CAPTURE_MAX_FRAMES.saturating_sub(at));
    if stage >= DEBUG_STAGES.len() || n == 0 {
      return;
    }
    // SAFETY: `at + n <= MAX_FRAMES` and `stage` in bounds; only the owning stream writes.
    unsafe {
      #[expect(clippy::indexing_slicing, reason = "stage < DEBUG_STAGES.len(), checked above")]
      let base = std::ptr::addr_of_mut!((*self.ptr.as_ptr()).audio[stage]) as *mut f32;
      std::ptr::copy_nonoverlapping(samples.as_ptr(), base.add(at), n);
    }
  }

  #[cfg(feature = "debug-capture")]
  fn write_zeros(&self, stage: usize, at: usize, len: usize) {
    let n = len.min(DEBUG_CAPTURE_MAX_FRAMES.saturating_sub(at));
    if stage >= DEBUG_STAGES.len() || n == 0 {
      return;
    }
    // SAFETY: as `write_audio`.
    unsafe {
      #[expect(clippy::indexing_slicing, reason = "stage < DEBUG_STAGES.len(), checked above")]
      let base = std::ptr::addr_of_mut!((*self.ptr.as_ptr()).audio[stage]) as *mut f32;
      std::ptr::write_bytes(base.add(at), 0, n);
    }
  }

  #[cfg(feature = "debug-capture")]
  fn write_row(&self, row: usize, values: &[f32; DEBUG_VARS.len()]) {
    if row >= DEBUG_CAPTURE_MAX_BLOCKS {
      return;
    }
    // SAFETY: row in bounds; only the owning stream writes.
    unsafe {
      let base = std::ptr::addr_of_mut!((*self.ptr.as_ptr()).vars) as *mut [f32; DEBUG_VARS.len()];
      base.add(row).write(*values);
    }
  }
}

impl Drop for DebugCaptureMap {
  fn drop(&mut self) {
    // SAFETY: the view and handle are owned by `self`.
    unsafe {
      let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.ptr.as_ptr() as *mut _ });
      let _ = CloseHandle(self.handle);
    }
  }
}

/// Feeds one capture from the mic chain: `begin_block`, the stages, `end_block`.
#[cfg(feature = "debug-capture")]
pub struct CaptureWriter {
  map: DebugCaptureMap,
  device_rate: u32,
  owning: bool,
  /// First frame of the current block, its length, and the capture's length.
  at: usize,
  len: usize,
  target: usize,
  start_qpc_hns: Option<u64>,
  /// Scratch for the reference at mic time (the chain has no buffer for it).
  pub scratch: Vec<f32>,
}

#[cfg(feature = "debug-capture")]
impl CaptureWriter {
  pub fn new(map: DebugCaptureMap, device_rate: u32) -> Self {
    Self { map, device_rate, owning: false, at: 0, len: 0, target: 0, start_qpc_hns: None, scratch: vec![0.0; 4096] }
  }

  /// Claims a requested capture; true while this stream records (then call the stages + `end_block`).
  pub fn begin_block(&mut self, frames: usize, qpc_hns: Option<u64>) -> bool {
    let h = self.map.header();
    let state = h.state.load(Ordering::Acquire);
    if self.owning && state != STATE_RECORDING {
      self.owning = false; // the tool cancelled or restarted it
    }
    if !self.owning
      && state == STATE_REQUESTED
      && h.state.compare_exchange(STATE_REQUESTED, STATE_RECORDING, Ordering::AcqRel, Ordering::Relaxed).is_ok()
    {
      self.owning = true;
      let target = h.target_frames.load(Ordering::Relaxed) as usize;
      self.target = if target == 0 { DEBUG_CAPTURE_MAX_FRAMES } else { target.min(DEBUG_CAPTURE_MAX_FRAMES) };
      self.start_qpc_hns = qpc_hns;
      h.frames.store(0, Ordering::Relaxed);
      h.blocks.store(0, Ordering::Relaxed);
      h.device_rate.store(self.device_rate, Ordering::Relaxed);
      h.writer_pid.store(std::process::id(), Ordering::Relaxed);
      h.aec_pre_delay_samples.store(crate::aec3::PRE_DELAY_SAMPLES as u32, Ordering::Relaxed);
      h.corr_max_delay_samples.store(crate::DelayEstimator::max_delay_samples() as u32, Ordering::Relaxed);
      h.start_qpc_hns.store(qpc_hns.unwrap_or(0), Ordering::Relaxed);
    }
    if !self.owning {
      return false;
    }
    self.at = h.frames.load(Ordering::Relaxed) as usize;
    self.len = frames.min(self.target.saturating_sub(self.at));
    if self.scratch.len() < frames {
      self.scratch.resize(frames, 0.0); // debug build only: a block larger than 85 ms
    }
    true
  }

  pub fn stage(&self, stage: usize, samples: &[f32]) {
    self.map.write_audio(stage, self.at, samples.get(..self.len).unwrap_or(samples));
  }

  pub fn stage_silent(&self, stage: usize) {
    self.map.write_zeros(stage, self.at, self.len);
  }

  /// Writes the reference at mic time from `scratch`.
  pub fn stage_scratch(&self, stage: usize) {
    self.map.write_audio(stage, self.at, self.scratch.get(..self.len).unwrap_or(&[]));
  }

  /// Milliseconds since the capture's first block, -1 without timestamps.
  pub fn t_ms(&self, qpc_hns: Option<u64>) -> f32 {
    match (qpc_hns, self.start_qpc_hns) {
      (Some(t), Some(t0)) => (t as i64 - t0 as i64) as f32 / 10_000.0,
      _ => -1.0,
    }
  }

  pub fn frame(&self) -> usize {
    self.at
  }

  pub fn end_block(&mut self, vars: &[f32; DEBUG_VARS.len()]) {
    let h = self.map.header();
    let blocks = h.blocks.load(Ordering::Relaxed);
    self.map.write_row(blocks as usize, vars);
    let frames = self.at + self.len;
    h.blocks.store(blocks.saturating_add(1), Ordering::Relaxed);
    h.frames.store(frames as u32, Ordering::Release);
    if frames >= self.target || blocks as usize + 1 >= DEBUG_CAPTURE_MAX_BLOCKS {
      self.owning = false;
      let _ = h.state.compare_exchange(STATE_RECORDING, STATE_DONE, Ordering::AcqRel, Ordering::Relaxed);
    }
  }
}

#[cfg(all(test, feature = "debug-capture"))]
mod tests {
  use super::*;

  #[test]
  fn test_claims_request_records_blocks_and_finishes_at_target() {
    let name = format!("Local\\GamersEqualizer_DebugCapture_test_{}", std::process::id());
    let map = DebugCaptureMap::create_named(&name, None).unwrap();
    let reader = DebugCaptureMap::create_named(&name, None).unwrap();
    let mut writer = CaptureWriter::new(map, 44_100);

    assert!(!writer.begin_block(480, Some(0)), "idle: nothing recorded");
    let h = reader.header();
    h.target_frames.store(1000, Ordering::Relaxed);
    h.state.store(STATE_REQUESTED, Ordering::Release);

    let mut row = [0.0f32; DEBUG_VARS.len()];
    for block in 0..3 {
      assert!(writer.begin_block(480, Some(block * 100_000)));
      writer.stage(STAGE_MIC_RAW, &vec![block as f32 + 1.0; 480]);
      writer.stage_silent(STAGE_REF_AEC);
      row[0] = writer.frame() as f32;
      writer.end_block(&row);
    }
    assert_eq!(h.state.load(Ordering::Acquire), STATE_DONE);
    assert_eq!(h.frames.load(Ordering::Relaxed), 1000, "last block truncated to the target");
    assert_eq!(h.device_rate.load(Ordering::Relaxed), 44_100);
    assert!(!writer.begin_block(480, None), "done: the writer lets go");

    let raw = reader.stage(STAGE_MIC_RAW, 1000);
    assert_eq!((raw[0], raw[479], raw[480], raw[999]), (1.0, 1.0, 2.0, 3.0));
    assert!(reader.stage(STAGE_REF_AEC, 1000).iter().all(|&s| s == 0.0));
    let rows: Vec<f32> = reader.vars(3).iter().map(|r| r[0]).collect();
    assert_eq!(rows, [0.0, 480.0, 960.0]);
  }
}
