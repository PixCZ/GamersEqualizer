//! Feature `cpu-stats`: the time our effects take inside audiodg, where nothing else can see
//! it. An APO built with the feature creates this Global mapping and adds every block's time
//! to it; a service built with the feature opens it and writes the numbers into the debug log
//! (`gamersequalizer-daemon` `cpu_stats.rs`). Never ship a build with it.

use crate::shared_memory::{create_mapping, wide};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
  FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
};
use windows::core::PCWSTR;

/// The version in the name follows the layout of `CpuStats`.
const CPU_STATS_NAME: &str = "Global\\GamersEqualizer_CpuStats_v1";

/// Time spent in one kind of effect, all its running instances together. All zeros (a fresh
/// mapping) is "nothing ran yet".
#[repr(C)]
pub struct EffectTime {
  /// Time inside the effect's process call since audiodg started, wall clock.
  pub busy_ns: AtomicU64,
  pub blocks: AtomicU64,
  /// The longest block since the reader last took it (`swap(0)`).
  pub longest_ns: AtomicU64,
}

#[repr(C)]
pub struct CpuStats {
  pub mic: EffectTime,
  pub output: EffectTime,
  pub stream: EffectTime,
}

pub struct CpuStatsMap {
  handle: HANDLE,
  ptr: NonNull<CpuStats>,
}

// SAFETY: process-wide memory that holds atomics only; the handle may be closed from any thread.
unsafe impl Send for CpuStatsMap {}
// SAFETY: as `Send`: every access goes through the atomics.
unsafe impl Sync for CpuStatsMap {}

impl EffectTime {
  /// One processed block. Lock-free: called on the audio thread.
  pub fn add(&self, ns: u64) {
    self.busy_ns.fetch_add(ns, Ordering::Relaxed);
    self.blocks.fetch_add(1, Ordering::Relaxed);
    self.longest_ns.fetch_max(ns, Ordering::Relaxed);
  }
}

impl CpuStatsMap {
  /// Create (or open) the Global mapping; `sddl` must let the logged-on user open it.
  pub fn create(sddl: &str) -> Result<Self, String> {
    let handle = create_mapping(CPU_STATS_NAME, Some(sddl), size_of::<CpuStats>())?;
    // SAFETY: a mapping of the layout's size that this call owns.
    unsafe { Self::map(handle) }
  }

  /// Open the mapping a `cpu-stats` APO created (fails until such an APO has run a stream).
  pub fn open_existing() -> Result<Self, String> {
    let name = wide(CPU_STATS_NAME);
    // SAFETY: NUL-terminated name owned by this frame; the mapping has the layout's size.
    unsafe {
      let handle = OpenFileMappingW((FILE_MAP_READ | FILE_MAP_WRITE).0, false, PCWSTR(name.as_ptr()))
        .map_err(|e| format!("OpenFileMappingW({CPU_STATS_NAME}) failed: {e}"))?;
      Self::map(handle)
    }
  }

  pub fn stats(&self) -> &CpuStats {
    // SAFETY: the view is a whole `CpuStats` and outlives `self`; it holds atomics only.
    unsafe { self.ptr.as_ref() }
  }

  /// Takes ownership of `handle` (closed on failure).
  ///
  /// # Safety
  /// `handle` is a file mapping handle of at least `CpuStats`' size that the caller owns and
  /// does not close itself.
  unsafe fn map(handle: HANDLE) -> Result<Self, String> {
    // SAFETY: the caller hands the handle over (contract above). The view is the layout's size or
    // the call fails; it and the handle are released once, in `Drop`, or the handle here on failure.
    unsafe {
      let view = MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, size_of::<CpuStats>());
      let Some(ptr) = NonNull::new(view.Value as *mut CpuStats) else {
        let _ = CloseHandle(handle);
        return Err("MapViewOfFile returned null pointer".to_string());
      };
      Ok(Self { handle, ptr })
    }
  }
}

impl Drop for CpuStatsMap {
  fn drop(&mut self) {
    // SAFETY: the view and the handle come from `map` and are released only here.
    unsafe {
      let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.ptr.as_ptr() as *mut _ });
      let _ = CloseHandle(self.handle);
    }
  }
}
