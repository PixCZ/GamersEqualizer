//! Channel levels the service changed (the tray flyout's Game / Chat / Media / Mic rows), for the
//! window to adopt. The window writes every level into the shared header each frame, so a level
//! the service put there alone would be overwritten; this block is written by the service only and
//! counted, and the window adopts it once per new count. It never writes back, so nothing bounces.
//!
//! A mapping of its own (`Local\`, window and service only): a new field in `ShmHeader` would bump
//! the layout version and leave an installed microphone effect unreachable until it is reinstalled.

use crate::win_audio::chatmix::ChatMixController;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
  CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::core::HSTRING;

/// A new layout of `MixerEdits` gets a new name (a window of another build then sees none).
const NAME: &str = r"Local\GamersEqualizer_MixerEdits_v1";

/// Zero in a new mapping: count 0, nothing to adopt.
#[repr(C)]
#[derive(Default)]
pub struct MixerEdits {
  /// Bumped after the levels are written.
  count: AtomicU32,
  /// Game, chat, media, mic (the mic's wish may be above 1000).
  permille: [AtomicU32; 4],
  muted: [AtomicBool; 4],
}

/// The mapped block; unmapped on drop.
pub struct MixerEditsMap {
  handle: HANDLE,
  view: NonNull<MixerEdits>,
}

impl MixerEdits {
  /// The service: the four channel levels as they are now in `mix`.
  pub fn publish(&self, mix: &ChatMixController) {
    for (k, c) in [&mix.game, &mix.chat, &mix.media, &mix.mic].into_iter().enumerate() {
      if let (Some(v), Some(m)) = (self.permille.get(k), self.muted.get(k)) {
        v.store((c.volume.max(0.0) * 1000.0).round() as u32, Ordering::Relaxed);
        m.store(c.muted, Ordering::Relaxed);
      }
    }
    self.count.fetch_add(1, Ordering::Release);
  }

  /// What the window saw last; at its start the count as it is (the header has those levels).
  pub fn count(&self) -> u32 {
    self.count.load(Ordering::Acquire)
  }

  /// The window: takes the levels into `mix` when the count moved since `seen`. Two edits between
  /// two frames leave the second one (the levels are read after the count).
  pub fn adopt(&self, seen: &mut u32, mix: &mut ChatMixController) -> bool {
    let now = self.count();
    if now == *seen {
      return false;
    }
    *seen = now;
    for (k, c) in [&mut mix.game, &mut mix.chat, &mut mix.media, &mut mix.mic].into_iter().enumerate() {
      if let (Some(v), Some(m)) = (self.permille.get(k), self.muted.get(k)) {
        c.volume = v.load(Ordering::Relaxed) as f32 / 1000.0;
        c.muted = m.load(Ordering::Relaxed);
      }
    }
    true
  }
}

impl MixerEditsMap {
  /// Opens the block, or makes it (zeroed) when neither process has it yet.
  pub fn open() -> Result<Self, String> {
    Self::open_named(NAME)
  }

  fn open_named(name: &str) -> Result<Self, String> {
    let size = size_of::<MixerEdits>();
    // SAFETY: a pagefile-backed mapping of `size` bytes under a static name; the view is checked for
    // null and the handle closed on that path. Windows zeroes a new mapping, which is a valid
    // `MixerEdits` (atomics only); both processes touch it through atomics alone.
    unsafe {
      let handle = CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, size as u32, &HSTRING::from(name))
        .map_err(|e| format!("CreateFileMappingW: {e}"))?;
      let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size);
      match NonNull::new(view.Value.cast::<MixerEdits>()) {
        Some(view) => Ok(Self { handle, view }),
        None => {
          let _ = CloseHandle(handle);
          Err("MapViewOfFile failed".into())
        }
      }
    }
  }

  pub fn edits(&self) -> &MixerEdits {
    // SAFETY: the view stays mapped until `self` drops; the block holds atomics only.
    unsafe { self.view.as_ref() }
  }
}

impl Drop for MixerEditsMap {
  fn drop(&mut self) {
    // SAFETY: the view and handle `open` made, released once here.
    unsafe {
      let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.as_ptr().cast() });
      let _ = CloseHandle(self.handle);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_window_adopts_a_flyout_edit_once_and_never_bounces_it() {
    let edits = MixerEdits::default();
    let (mut service, mut window) = (ChatMixController::new(), ChatMixController::new());
    let mut seen = edits.count();
    assert!(!edits.adopt(&mut seen, &mut window), "nothing edited yet");
    (service.chat.volume, service.mic.volume, service.media.muted) = (0.3, 2.5, true);
    edits.publish(&service);
    assert!(edits.adopt(&mut seen, &mut window));
    assert_eq!((window.chat.volume, window.mic.volume, window.media.muted), (0.3, 2.5, true));
    // The window moves its slider next: the block does not hand the old level back.
    window.chat.volume = 0.8;
    assert!(!edits.adopt(&mut seen, &mut window));
    assert_eq!(window.chat.volume, 0.8);
  }

  #[test]
  fn two_edits_between_frames_leave_the_last_one() {
    let edits = MixerEdits::default();
    let (mut service, mut window) = (ChatMixController::new(), ChatMixController::new());
    let mut seen = edits.count();
    service.game.volume = 0.2;
    edits.publish(&service);
    service.game.volume = 0.6;
    service.game.muted = true;
    edits.publish(&service);
    assert!(edits.adopt(&mut seen, &mut window));
    assert_eq!((window.game.volume, window.game.muted), (0.6, true));
  }

  #[test]
  fn both_processes_see_the_same_block() {
    // Not the running app's block.
    let name = format!(r"Local\GamersEqualizer_MixerEdits_test_{}", std::process::id());
    let (a, b) = (MixerEditsMap::open_named(&name).unwrap(), MixerEditsMap::open_named(&name).unwrap());
    let mut seen = b.edits().count();
    let mut service = ChatMixController::new();
    service.media.volume = 0.45;
    a.edits().publish(&service);
    let mut window = ChatMixController::new();
    assert!(b.edits().adopt(&mut seen, &mut window));
    assert_eq!(window.media.volume, 0.45);
  }
}
