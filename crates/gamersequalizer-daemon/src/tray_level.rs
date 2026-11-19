//! The tray icon's bars follow the microphone level: "does my mic work, am I heard?" at a glance.
//! The level is the mic chain's output (what the other side gets), so the bars move only while
//! our effect processes the microphone. Every change is one icon sent to Explorer, so it is
//! rationed: a few frames a second, only when the bars change by a step, and not at all while
//! nobody can see the icon (a full-screen app in front, the icon in the overflow, switched off).

use crate::ui::prefs::{self, UiPrefs};
use gamersequalizer_common::shm::ShmHeader;
use log::info;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};
use tray_icon::Rect;
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext};
use windows::Win32::UI::Shell::{
  QUNS_BUSY, QUNS_NOT_PRESENT, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowRect};
use windows::core::{PCWSTR, w};

/// The bars change at most this often (the service loop adds up to one of its steps).
const FRAME_INTERVAL: Duration = Duration::from_millis(200);
/// Bar heights the level is rounded to; an icon is sent only when the step changes.
const STEPS: u8 = 8;
/// The chain's output level (dBFS, RMS) of the lowest and of the full bars. Full is the voice
/// leveler's default target, so normal speech draws the icon the way it looks standing still.
const FLOOR_DB: f32 = -60.0;
const FULL_DB: f32 = -24.0;
/// The bars rise at once and fall this many steps per frame.
const FALL_STEPS: u8 = 2;
/// Bar height at step 0 as a share of the full one: a silent, working mic still shows bars.
const MIN_SCALE: f32 = 0.2;
/// The mic effect counts as running this long after its last telemetry (it comes every ~100 ms).
const LIVE_WINDOW: Duration = Duration::from_secs(1);

pub struct LevelMeter {
  prefs_dir: PathBuf,
  /// Modification time of the preferences file when `off` was read from it.
  prefs_changed: Option<SystemTime>,
  /// The user switched the feature off in Settings.
  off: bool,
  /// The last check allows the bars to move.
  follows: bool,
  /// What the last check found, as logged; empty while no app records.
  status: &'static str,
  seq: u32,
  live_at: Option<Instant>,
  /// Highest level since the last frame, dBFS x 10.
  peak_db_x10: i32,
  frame_at: Option<Instant>,
  step: Option<u8>,
}

impl LevelMeter {
  pub fn new(prefs_dir: &Path) -> Self {
    Self {
      prefs_dir: prefs_dir.to_path_buf(),
      prefs_changed: None,
      off: false,
      follows: false,
      status: "",
      seq: 0,
      live_at: None,
      peak_db_x10: i32::MIN,
      frame_at: None,
      step: None,
    }
  }

  /// About once a second: may the bars follow the level right now? `icon_rect` asks Explorer,
  /// so it is called only when nothing else stops the bars.
  pub fn check(&mut self, mic_active: bool, warning: bool, icon_rect: impl FnOnce() -> Option<Rect>) {
    if !mic_active {
      // The tray logs "microphone free" itself.
      self.follows = false;
      self.status = "";
      return;
    }
    // The window saves the switch into its preferences file; only a changed file is read.
    let changed = std::fs::metadata(self.prefs_dir.join(prefs::FILE)).and_then(|m| m.modified()).ok();
    if changed != self.prefs_changed {
      self.prefs_changed = changed;
      self.off = UiPrefs::load_or_default(&self.prefs_dir).tray_mic_level_off;
    }
    let (follows, status, place) = if self.off {
      (false, "stands still: the microphone level is switched off in Settings", String::new())
    } else if warning {
      (false, "stands still: it shows a setup problem", String::new())
    } else if taskbar_out_of_sight() {
      (false, "stands still: a full-screen app is in front or the screen is locked", String::new())
    } else {
      icon_place(icon_rect)
    };
    self.follows = follows;
    // Compared without the position: an icon that only moved along the taskbar logs nothing.
    if status != self.status {
      info!("[Tray] icon {status}{place}");
      self.status = status;
    }
  }

  /// Every service-loop step: the bar step to show, None = the icon as it stands still.
  pub fn frame(&mut self, header: Option<&ShmHeader>) -> Option<u8> {
    let Some(h) = header.filter(|_| self.follows) else {
      self.frame_at = None;
      self.live_at = None;
      self.step = None;
      return None;
    };
    self.peak_db_x10 = self.peak_db_x10.max(h.aec_out_level_db_x10.load(Ordering::Relaxed));
    if self.frame_at.is_some_and(|at| at.elapsed() < FRAME_INTERVAL) {
      return self.shown();
    }
    let first = self.frame_at.replace(Instant::now()).is_none();
    let peak = std::mem::replace(&mut self.peak_db_x10, i32::MIN);
    // The service bumps its copy of the sequence only while the mic effect produces audio
    // (`ApoLink::sync`). The first frame has nothing to compare it with: it draws no old level.
    let seq = h.aec_telemetry_seq.load(Ordering::Acquire);
    if seq != std::mem::replace(&mut self.seq, seq) && !first {
      self.live_at = Some(Instant::now());
    }
    let live = self.live_at.is_some_and(|at| at.elapsed() < LIVE_WINDOW);
    self.step = live.then(|| next_step(self.step, peak as f32 / 10.0));
    self.shown()
  }

  /// `step` for the icon. The full step draws the standing icon, so it is None: no icon is
  /// sent when speech at full level starts or ends.
  fn shown(&self) -> Option<u8> {
    self.step.filter(|&s| s < STEPS)
  }
}

/// Bar height for `app_icon::rgba`: 1.0 while the icon stands still.
pub fn bar_scale(step: Option<u8>) -> f32 {
  step.map_or(1.0, |s| MIN_SCALE + (1.0 - MIN_SCALE) * f32::from(s.min(STEPS)) / f32::from(STEPS))
}

/// The step for a level: up at once, down `FALL_STEPS` per frame.
fn next_step(shown: Option<u8>, level_db: f32) -> u8 {
  let share = ((level_db - FLOOR_DB) / (FULL_DB - FLOOR_DB)).clamp(0.0, 1.0);
  let target = (share * f32::from(STEPS)).round() as u8;
  target.max(shown.unwrap_or(0).saturating_sub(FALL_STEPS))
}

/// A full-screen app (also exclusive Direct3D, presentation mode) covers the taskbar; a locked
/// screen or a screensaver hides it. With the game on a second monitor this still says yes:
/// the bars then stand still although visible.
fn taskbar_out_of_sight() -> bool {
  // SAFETY: no arguments; the shell returns the state by value.
  let state = unsafe { SHQueryUserNotificationState() };
  let hidden = [QUNS_NOT_PRESENT, QUNS_BUSY, QUNS_RUNNING_D3D_FULL_SCREEN, QUNS_PRESENTATION_MODE];
  state.is_ok_and(|s| hidden.contains(&s))
}

/// Whether the icon sits on the taskbar (not in the hidden-icons flyout), the status to log
/// and the positions behind it. Unknown counts as visible: the bars move and the log says why.
fn icon_place(icon_rect: impl FnOnce() -> Option<Rect>) -> (bool, &'static str, String) {
  const FOLLOWS: &str = "follows the microphone level";
  let mut bar = RECT::default();
  // The service declares no DPI awareness, so both positions are asked for as a per-monitor
  // aware thread: they then share one coordinate space at any display scaling.
  // SAFETY: a system constant; returns the context to put back (null on failure, and setting null fails harmlessly).
  let before = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
  let icon = icon_rect();
  // SAFETY: a static class name; `bar` is a local the call fills and stays empty when a call fails; then the
  // thread gets its earlier context back.
  let found = unsafe {
    let found = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).is_ok_and(|h| GetWindowRect(h, &mut bar).is_ok());
    SetThreadDpiAwarenessContext(before);
    found
  };
  let Some(icon) = icon else {
    return (true, FOLLOWS, " (Windows gave no position for the icon)".to_string());
  };
  let x = icon.position.x + f64::from(icon.size.width) / 2.0;
  let y = icon.position.y + f64::from(icon.size.height) / 2.0;
  let RECT { left, top, right, bottom } = bar;
  let on_bar = (f64::from(left)..f64::from(right)).contains(&x) && (f64::from(top)..f64::from(bottom)).contains(&y);
  let place = format!(" (icon at {x:.0},{y:.0}; taskbar {left},{top} to {right},{bottom})");
  if !found || on_bar { (true, FOLLOWS, place) } else { (false, "stands still: it is among the hidden icons", place) }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bars_rise_at_once_and_fall_by_steps() {
    assert_eq!(next_step(None, FULL_DB), STEPS);
    // Louder than full and quieter than the floor stay in range.
    assert_eq!(next_step(Some(0), 0.0), STEPS);
    assert_eq!(next_step(None, -120.0), 0);
    assert_eq!(next_step(None, (FLOOR_DB + FULL_DB) / 2.0), STEPS / 2);
    // Silence after speech: down by steps, not at once.
    assert_eq!(next_step(Some(STEPS), -120.0), STEPS - FALL_STEPS);
    assert_eq!(next_step(Some(1), -120.0), 0);
  }

  #[test]
  fn full_step_draws_the_standing_icon_and_silence_keeps_short_bars() {
    assert!((bar_scale(Some(STEPS)) - bar_scale(None)).abs() < 1e-6);
    assert!((bar_scale(Some(0)) - MIN_SCALE).abs() < 1e-6);
  }
}
