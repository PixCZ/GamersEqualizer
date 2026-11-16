//! Remembers the window's size, position and maximized state in
//! `%APPDATA%\GamersEqualizer\profiles\window.json` (no extra dependency: eframe's own
//! persistence would pull in `ron`). A position that is no longer on any monitor (e.g. a
//! disconnected second screen) is dropped, so the window never opens invisible.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::Win32::UI::WindowsAndMessaging::{
  GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

pub const DEFAULT_SIZE: [f32; 2] = [1180.0, 720.0];
pub const MIN_SIZE: [f32; 2] = [720.0, 440.0];
const FILE: &str = "window.json";
/// Save once the window has not moved/resized for this long (a drag fires every frame).
const SAVE_DEBOUNCE: Duration = Duration::from_millis(800);
/// At least this much of the window (in points) must lie on the screens.
const MIN_VISIBLE: f32 = 120.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
  /// Outer top-left corner, in points.
  pub x: f32,
  pub y: f32,
  /// Inner (content) size, in points.
  pub width: f32,
  pub height: f32,
  pub maximized: bool,
}

/// Tracks the live geometry and saves it debounced.
pub struct WindowTracker {
  path: PathBuf,
  /// Last geometry of the *normal* (not maximized) window plus the maximized flag.
  current: Option<WindowGeometry>,
  saved: Option<WindowGeometry>,
  changed_at: Option<Instant>,
}

impl WindowGeometry {
  /// Missing / corrupt file or off-screen position -> None (defaults are used).
  pub fn load(dir: &Path) -> Option<Self> {
    let text = std::fs::read_to_string(dir.join(FILE)).ok()?;
    let g: Self = serde_json::from_str(&text).ok()?;
    let finite = [g.x, g.y, g.width, g.height].iter().all(|v| v.is_finite());
    finite.then(|| g.sanitized(virtual_screen()))
  }

  /// Size clamped to the minimum; position dropped (NaN) when it would be off-screen.
  fn sanitized(mut self, screen: Option<[f32; 4]>) -> Self {
    self.width = self.width.max(MIN_SIZE[0]);
    self.height = self.height.max(MIN_SIZE[1]);
    if let Some([sx, sy, sw, sh]) = screen {
      let visible_x = (self.x + self.width).min(sx + sw) - self.x.max(sx);
      let visible_y = (self.y + 40.0).min(sy + sh) - self.y.max(sy); // title bar must be reachable
      if visible_x < MIN_VISIBLE || visible_y < 20.0 {
        self.x = f32::NAN;
        self.y = f32::NAN;
      }
    }
    self
  }

  pub fn has_position(&self) -> bool {
    self.x.is_finite() && self.y.is_finite()
  }
}

impl WindowTracker {
  pub fn new(dir: &Path, initial: Option<WindowGeometry>) -> Self {
    Self { path: dir.join(FILE), current: initial, saved: initial, changed_at: None }
  }

  /// Call every frame with the viewport info.
  pub fn observe(&mut self, ctx: &egui::Context) {
    let (inner, outer, maximized) = ctx.input(|i| {
      let vp = i.viewport();
      (vp.inner_rect, vp.outer_rect, vp.maximized.unwrap_or(false))
    });
    let (Some(inner), Some(outer)) = (inner, outer) else {
      return;
    };
    let next = match (maximized, self.current) {
      // Keep the normal size/position while maximized, so un-maximizing restores it.
      (true, Some(prev)) => WindowGeometry { maximized: true, ..prev },
      _ => WindowGeometry { x: outer.min.x, y: outer.min.y, width: inner.width(), height: inner.height(), maximized },
    };
    if self.current != Some(next) {
      self.current = Some(next);
      self.changed_at = Some(Instant::now());
    }
    if self.changed_at.is_some_and(|t| t.elapsed() >= SAVE_DEBOUNCE) {
      self.save();
    }
  }

  /// Write if changed since the last save (also called on exit).
  pub fn save(&mut self) {
    self.changed_at = None;
    let Some(g) = self.current.filter(|g| Some(*g) != self.saved) else {
      return;
    };
    let result = serde_json::to_string_pretty(&g)
      .map_err(std::io::Error::other)
      .and_then(|json| crate::output_eq::settings::write_atomic(&self.path, &json));
    match result {
      Ok(()) => self.saved = Some(g),
      Err(e) => log::warn!("[Window] could not save {}: {e}", self.path.display()),
    }
  }
}

/// [x, y, width, height] of all monitors together, in points like the saved position: `load` runs
/// before winit makes the process DPI aware, and until then Windows scales these to 96 DPI. With
/// two monitors at different scaling it is approximate; enough for "is it visible at all".
fn virtual_screen() -> Option<[f32; 4]> {
  // SAFETY: plain metric queries without pointers.
  let (x, y, w, h) = unsafe {
    (
      GetSystemMetrics(SM_XVIRTUALSCREEN),
      GetSystemMetrics(SM_YVIRTUALSCREEN),
      GetSystemMetrics(SM_CXVIRTUALSCREEN),
      GetSystemMetrics(SM_CYVIRTUALSCREEN),
    )
  };
  (w > 0 && h > 0).then_some([x as f32, y as f32, w as f32, h as f32])
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn off_screen_position_is_dropped_and_size_clamped() {
    let screen = Some([0.0, 0.0, 1920.0, 1080.0]);
    let on = WindowGeometry { x: 100.0, y: 50.0, width: 900.0, height: 600.0, maximized: false }.sanitized(screen);
    assert!(on.has_position());
    // Was on a second monitor at x = 2500 that is now unplugged.
    let off = WindowGeometry { x: 2500.0, y: 50.0, width: 900.0, height: 600.0, maximized: false }.sanitized(screen);
    assert!(!off.has_position());
    let tiny = WindowGeometry { x: 0.0, y: 0.0, width: 10.0, height: 10.0, maximized: true }.sanitized(screen);
    assert_eq!((tiny.width, tiny.height), (MIN_SIZE[0], MIN_SIZE[1]));
  }

  #[test]
  fn save_and_load_roundtrip_and_corrupt_file() {
    let dir = std::env::temp_dir().join(format!("ge_window_test_{}", std::process::id()));
    let g = WindowGeometry { x: 10.0, y: 20.0, width: 1000.0, height: 700.0, maximized: true };
    let mut t = WindowTracker::new(&dir, None);
    t.current = Some(g);
    t.save();
    let loaded = WindowGeometry::load(&dir).unwrap();
    assert_eq!((loaded.width, loaded.height, loaded.maximized), (1000.0, 700.0, true));
    std::fs::write(dir.join(FILE), "garbage").unwrap();
    assert!(WindowGeometry::load(&dir).is_none());
    let _ = std::fs::remove_dir_all(&dir);
  }
}
