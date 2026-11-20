//! Notification-area icon, owned by the background service (it runs whenever the app does).
//! Closing the window therefore "hides to tray"; the icon reopens it or exits everything.
//! Setup problems (`health`) put a badge on the icon and list themselves, with fixes, at the
//! top of the menu.

use crate::health::{Fix, Issue};
use crate::mic_usage::MicStoreWatch;
use crate::tray_level::{self, LevelMeter};
use crate::win_audio::worker::RepeatFilter;
use gamersequalizer_common::shm::ShmHeader;
use log::{info, warn};
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{
  CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer, TIMER_ALL_ACCESS,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
  DispatchMessageW, FindWindowW, GetForegroundWindow, IsIconic, MSG, MsgWaitForMultipleObjects, PM_REMOVE,
  PeekMessageW, PostMessageW, QS_ALLINPUT, SW_RESTORE, SW_SHOW, SW_SHOWNORMAL, SetForegroundWindow, ShowWindow,
  TranslateMessage, WM_CLOSE,
};
use windows::core::{HSTRING, PCWSTR, w};

/// Title of the main window; also how the service finds it (focus, close on exit).
pub const WINDOW_TITLE: &str = "GamersEqualizer - Audio Control Center";
/// At logon the service can start before Explorer's notification area exists; retry until it does.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CREATE_NO_WINDOW: u32 = 0x08000000;
/// How often the icon checks whether an app records from a microphone (a few registry reads).
/// Since Windows announces each change (`MicStoreWatch`) this is the safety net: a missed
/// announcement, a key that could not be watched or was recreated.
const MIC_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// Announced changes are checked at most this often. Bounds a flood (an app that rewrites its
/// timestamps all the time): the watch is re-armed only when its change is checked, so at most one
/// wake from the announcement and one at the end of this gap, each scan at most every 100 ms.
const MIC_RESCAN_GAP: Duration = Duration::from_millis(100);
const TRAY_ICON_SIZE: u32 = 32;
const TOOLTIP: &str = "GamersEqualizer";
const TOOLTIP_MIC: &str = "GamersEqualizer - microphone in use";
/// `NOTIFYICONDATAW::szTip` holds 128 UTF-16 units including the terminator.
const TOOLTIP_MAX_UNITS: usize = 127;

/// What the user asked for through the tray icon.
pub enum TrayAction {
  Exit,
  /// A fix from the setup-problems part of the menu.
  Fix(Fix),
}

pub struct TrayManager {
  tray_icon: TrayIcon,
  item_open_id: muda::MenuId,
  item_updates_id: muda::MenuId,
  item_quit_id: muda::MenuId,
  /// Menu items under the listed issues and what they do.
  fix_items: Vec<(muda::MenuId, Fix)>,
}

/// Keeps the tray icon alive in the service loop, creating it lazily.
pub struct TrayHost {
  tray: Option<TrayManager>,
  last_attempt: Option<Instant>,
  mic_active: bool,
  mic_checked_at: Option<Instant>,
  /// None while ConsentStore\microphone cannot be watched (reopened on the 1 s check).
  mic_watch: Option<MicStoreWatch>,
  watch_error: RepeatFilter,
  /// An announced change not checked yet (`MIC_RESCAN_GAP`), and when the last one was.
  mic_store_changed: bool,
  mic_forced_at: Option<Instant>,
  /// Setup problems: badge, tooltip lines and the top of the menu.
  issues: Vec<Issue>,
  /// The bars follow the microphone level; `level_shown` is the step the icon has now.
  level: LevelMeter,
  level_shown: Option<u8>,
}

impl TrayManager {
  pub fn new() -> Result<Self, String> {
    let tray_icon = TrayIconBuilder::new()
      // Left click opens the window; the menu is on right click.
      .with_menu_on_left_click(false)
      .with_tooltip(TOOLTIP)
      .with_icon(tray_icon(false, false, None)?)
      .build()
      .map_err(|e| format!("Failed to create tray icon: {}", e))?;
    let mut tray = Self {
      tray_icon,
      item_open_id: muda::MenuId::default(),
      item_updates_id: muda::MenuId::default(),
      item_quit_id: muda::MenuId::default(),
      fix_items: Vec::new(),
    };
    tray.set_menu(&[])?;
    Ok(tray)
  }

  /// Rebuilds the menu: each issue as a greyed title with its fixes under it, then the usual items.
  fn set_menu(&mut self, issues: &[Issue]) -> Result<(), String> {
    let menu_err = |e: muda::Error| format!("Failed to build tray menu: {}", e);
    let menu = Menu::new();
    let mut fix_items = Vec::new();
    for issue in issues {
      menu.append(&MenuItem::new(format!("⚠ {}", issue.title()), false, None)).map_err(menu_err)?;
      for (label, fix) in issue.fixes() {
        let item = MenuItem::new(format!("      {label}"), true, None);
        menu.append(&item).map_err(menu_err)?;
        fix_items.push((item.id().clone(), fix));
      }
      menu.append(&PredefinedMenuItem::separator()).map_err(menu_err)?;
    }

    let item_open = MenuItem::new(crate::i18n::tr("Open GamersEqualizer"), true, None);
    let item_updates = MenuItem::new(crate::i18n::tr("Check for updates (GitHub)..."), true, None);
    let item_quit = MenuItem::new(crate::i18n::tr("Exit GamersEqualizer"), true, None);
    menu
      .append_items(&[
        &item_open,
        &PredefinedMenuItem::separator(),
        &item_updates,
        &PredefinedMenuItem::separator(),
        &item_quit,
      ])
      .map_err(menu_err)?;

    self.tray_icon.set_menu(Some(Box::new(menu)));
    self.item_open_id = item_open.id().clone();
    self.item_updates_id = item_updates.id().clone();
    self.item_quit_id = item_quit.id().clone();
    self.fix_items = fix_items;
    Ok(())
  }

  /// Red icon while an app records from a microphone; badge while there are setup problems.
  fn show_state(&self, mic_active: bool, issues: &[Issue], level: Option<u8>) {
    self.set_icon(mic_active, !issues.is_empty(), level);
    let base = if mic_active { TOOLTIP_MIC } else { TOOLTIP };
    let lines: Vec<String> =
      std::iter::once(base.to_string()).chain(issues.iter().map(|i| format!("⚠ {}", i.title()))).collect();
    let _ = self.tray_icon.set_tooltip(Some(truncate_tooltip(&lines.join("\n"))));
  }

  /// The icon alone (a level frame leaves the tooltip as it is).
  fn set_icon(&self, mic_active: bool, warning: bool, level: Option<u8>) {
    match tray_icon(mic_active, warning, level) {
      Ok(icon) => {
        if let Err(e) = self.tray_icon.set_icon(Some(icon)) {
          warn!("[Tray] could not change the icon: {e}");
        }
      }
      Err(e) => warn!("[Tray] {e}"),
    }
  }

  /// Principle 1: Air-gapped update check via OS default browser
  pub fn open_updates_browser() {
    // SAFETY: static NUL-terminated strings (`w!`) and no window; the shell opens the link in the user's browser.
    unsafe {
      ShellExecuteW(
        None,
        w!("open"),
        w!("https://github.com/PixCZ/GamersEqualizer/releases"),
        None,
        None,
        SW_SHOWNORMAL,
      );
    }
  }
}

impl TrayHost {
  /// `prefs_dir`: where the window saves `ui_prefs.json` (the switch for the microphone level).
  pub fn new(prefs_dir: &Path) -> Self {
    Self {
      tray: None,
      last_attempt: None,
      mic_active: false,
      mic_checked_at: None,
      mic_watch: None,
      watch_error: RepeatFilter::default(),
      mic_store_changed: false,
      mic_forced_at: None,
      issues: Vec::new(),
      level: LevelMeter::new(prefs_dir),
      level_shown: None,
    }
  }

  /// New list of setup problems (from `HealthMonitor::tick`).
  pub fn set_issues(&mut self, issues: Vec<Issue>) {
    self.issues = issues;
    if let Some(tray) = self.tray.as_mut() {
      if let Err(e) = tray.set_menu(&self.issues) {
        warn!("[Tray] {e}");
      }
      // A badge covers the bars: the icon stands still, and the next step decides again.
      self.level_shown = None;
      self.mic_checked_at = None;
      tray.show_state(self.mic_active, &self.issues, None);
    }
  }

  /// One service-loop step: pumps this thread's window messages (the tray icon's hidden
  /// window lives on it), creates the icon if missing, moves its bars with the microphone
  /// level (`header`: the service's mapping) and handles clicks and menu items.
  pub fn poll(&mut self, header: Option<&ShmHeader>) -> Option<TrayAction> {
    pump_messages();

    if self.tray.is_none() && self.last_attempt.is_none_or(|t| t.elapsed() >= RETRY_INTERVAL) {
      let first = self.last_attempt.is_none();
      self.last_attempt = Some(Instant::now());
      match TrayManager::new() {
        Ok(mut t) => {
          info!("[Tray] icon created");
          // A new icon starts plain; recheck the mic right away and restore the problems.
          self.mic_active = false;
          self.mic_checked_at = None;
          self.level_shown = None;
          if !self.issues.is_empty() {
            if let Err(e) = t.set_menu(&self.issues) {
              warn!("[Tray] {e}");
            }
            t.show_state(false, &self.issues, None);
          }
          self.tray = Some(t);
        }
        // Warn once, then keep retrying quietly; the service works without the icon.
        Err(e) if first => warn!("[Tray] {e}; retrying every {}s", RETRY_INTERVAL.as_secs()),
        Err(_) => {}
      }
    }
    self.tray.as_ref()?;
    self.follow_mic_store();
    let mic_due = crate::win_audio::worker::due(self.mic_checked_at.map(|t| t.elapsed()), MIC_CHECK_INTERVAL);
    if mic_due && self.mic_watch.is_none() {
      self.open_mic_watch();
    }
    let tray = self.tray.as_ref()?;

    let (mut mic_changed, mut level) = (false, self.level_shown);
    if mic_due {
      self.mic_checked_at = Some(Instant::now());
      let active = crate::mic_usage::microphone_in_use();
      if active != self.mic_active {
        self.mic_active = active;
        info!("[Tray] microphone {}", if active { "in use" } else { "free" });
        mic_changed = true;
      }
      level = self.level.tick(header, active, !self.issues.is_empty(), || tray.tray_icon.rect());
    }
    // One icon at most: the colour and the bars go out together.
    let level_changed = level != std::mem::replace(&mut self.level_shown, level);
    if mic_changed {
      tray.show_state(self.mic_active, &self.issues, level);
    } else if level_changed {
      tray.set_icon(self.mic_active, !self.issues.is_empty(), level);
    }

    let mut action = None;
    while let Ok(event) = TrayIconEvent::receiver().try_recv() {
      if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
        show_or_launch_window();
      }
    }
    while let Ok(event) = MenuEvent::receiver().try_recv() {
      if event.id == tray.item_open_id {
        show_or_launch_window();
      } else if event.id == tray.item_updates_id {
        TrayManager::open_updates_browser();
      } else if event.id == tray.item_quit_id {
        action = Some(TrayAction::Exit);
      } else if let Some((_, fix)) = tray.fix_items.iter().find(|(id, _)| *id == event.id) {
        action = Some(TrayAction::Fix(fix.clone()));
      }
    }
    action
  }

  /// The idle wait of the service loop (`wait_for_messages`); Windows announcing a microphone
  /// opened or closed ends it too, and a change waiting for `MIC_RESCAN_GAP` shortens it.
  pub fn wait(&self, timeout: Duration) {
    let timeout = rescan_in(self.mic_store_changed, self.mic_forced_at.map(|t| t.elapsed()))
      .map_or(timeout, |left| timeout.min(left));
    wait_for_messages(timeout, self.mic_watch.as_ref().map(MicStoreWatch::event));
  }

  /// A change Windows announced: the mic is checked on this pass, without the scan cache, not on
  /// the next 1 s check. The loop stays idle; only the check's result can change that.
  fn follow_mic_store(&mut self) {
    let Some(watch) = self.mic_watch.as_ref() else { return };
    if watch.signaled() {
      self.mic_store_changed = true;
    }
    if rescan_in(self.mic_store_changed, self.mic_forced_at.map(|t| t.elapsed())) != Some(Duration::ZERO) {
      return;
    }
    // Armed before the scan: a write from here on is announced again.
    let armed = watch.arm();
    self.mic_store_changed = false;
    self.mic_forced_at = Some(Instant::now());
    self.mic_checked_at = None;
    crate::mic_usage::invalidate();
    if let Err(e) = armed {
      self.watch_failed(&e);
    }
  }

  fn open_mic_watch(&mut self) {
    match MicStoreWatch::new() {
      Ok(watch) => {
        info!("[Tray] Windows announces microphone use, no waiting for the {} s check", MIC_CHECK_INTERVAL.as_secs());
        self.watch_error.reset();
        self.mic_watch = Some(watch);
      }
      Err(e) => self.watch_failed(&e),
    }
  }

  /// Back to the 1 s check; it reopens the watch. The same error again is not logged again.
  fn watch_failed(&mut self, err: &str) {
    self.mic_watch = None;
    if self.watch_error.is_new(err) {
      warn!("[Tray] microphone changes are not announced ({err}); checking every {} s", MIC_CHECK_INTERVAL.as_secs());
    }
  }
}

/// The open main window, if any.
pub fn find_window() -> Option<HWND> {
  // SAFETY: a NUL-terminated title that outlives the call; a missing window is an error, not a handle.
  unsafe { FindWindowW(PCWSTR::null(), &HSTRING::from(WINDOW_TITLE)) }.ok().filter(|h| !h.is_invalid())
}

/// The main window is open and the active window (its own bar shows problems; no notification).
pub fn window_in_front() -> bool {
  // SAFETY: no arguments; the handle is only compared.
  find_window().is_some_and(|h| unsafe { GetForegroundWindow() } == h)
}

/// Brings an existing window to the front (restoring it if minimized). Returns false if none is open.
pub fn focus_window() -> bool {
  let Some(hwnd) = find_window() else { return false };
  // SAFETY: plain window calls on a handle from `find_window`; a window closed meanwhile only makes them fail.
  unsafe {
    let _ = ShowWindow(hwnd, if IsIconic(hwnd).as_bool() { SW_RESTORE } else { SW_SHOW });
    let _ = SetForegroundWindow(hwnd);
  }
  true
}

/// Asks the window to close normally (it saves its geometry on the way out).
pub fn close_window() {
  if let Some(hwnd) = find_window() {
    // SAFETY: posting to a handle from `find_window`; a window closed meanwhile only makes the call fail.
    unsafe {
      let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
  }
}

pub(crate) fn show_or_launch_window() {
  if focus_window() {
    return;
  }
  match std::env::current_exe() {
    Ok(exe) => {
      if let Err(e) = Command::new(exe).creation_flags(CREATE_NO_WINDOW).spawn() {
        warn!("[Tray] could not open the window: {e}");
      }
    }
    Err(e) => warn!("[Tray] could not locate the executable: {e}"),
  }
}

fn pump_messages() {
  let mut msg = MSG::default();
  // SAFETY: `msg` is a local that each call fills before the next one reads it.
  unsafe {
    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
      let _ = TranslateMessage(&msg);
      DispatchMessageW(&msg);
    }
  }
}

/// Sleeps up to `timeout`, but a new message for this thread (a click on the icon, its menu) ends
/// it at once: the long idle wait of the service loop keeps the icon as quick as before. A
/// high-resolution timer ends the wait: the call's own timeout ends on the system clock tick (up
/// to 15.6 ms late), off the wake grid the other service threads sleep on (`worker::grid_wait`).
/// `event`: one more handle that ends the wait when signaled (`MicStoreWatch`).
fn wait_for_messages(timeout: Duration, event: Option<HANDLE>) {
  let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
  // Relative due time in 100 ns units (negative = relative).
  let due = -i64::try_from(timeout.as_nanos() / 100).unwrap_or(i64::MAX);
  // SAFETY: the timer is created, armed with `due` (a local read during the call), waited on and
  // closed here, once; without it the wait falls back to the plain timeout. `event` is owned by the
  // caller's watch, which outlives the call.
  unsafe {
    let timer = CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0);
    let armed = timer.as_ref().ok().filter(|t| SetWaitableTimer(**t, &due, 0, None, None, false).is_ok()).copied();
    let handles: Vec<HANDLE> = armed.into_iter().chain(event).collect();
    // With the timer the timeout only backs it up.
    let ms = if armed.is_some() { ms.saturating_add(100) } else { ms };
    let _ = MsgWaitForMultipleObjects((!handles.is_empty()).then_some(handles.as_slice()), false, ms, QS_ALLINPUT);
    if let Ok(t) = timer {
      let _ = CloseHandle(t);
    }
  }
  #[cfg(feature = "cpu-stats")]
  crate::cpu_stats::woke();
}

fn tray_icon(mic_active: bool, warning: bool, level: Option<u8>) -> Result<Icon, String> {
  let rgba = crate::app_icon::rgba(TRAY_ICON_SIZE, mic_active, warning, tray_level::bar_scale(level));
  Icon::from_rgba(rgba, TRAY_ICON_SIZE, TRAY_ICON_SIZE).map_err(|e| format!("Icon creation failed: {}", e))
}

/// When an announced change is checked: None = none waiting, ZERO = now, else after that long, so
/// at most one forced check per `MIC_RESCAN_GAP`. `since_forced`: the last one, None = never.
fn rescan_in(changed: bool, since_forced: Option<Duration>) -> Option<Duration> {
  changed.then(|| since_forced.map_or(Duration::ZERO, |s| MIC_RESCAN_GAP.saturating_sub(s)))
}

/// Windows cuts tray tooltips at 127 UTF-16 units; cut at a char boundary with an ellipsis.
fn truncate_tooltip(text: &str) -> String {
  if text.encode_utf16().count() <= TOOLTIP_MAX_UNITS {
    return text.to_string();
  }
  let mut out = String::new();
  let mut units = 0;
  for c in text.chars() {
    if units + c.len_utf16() > TOOLTIP_MAX_UNITS - 1 {
      break;
    }
    units += c.len_utf16();
    out.push(c);
  }
  out.push('…');
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn long_tooltip_is_cut_to_the_windows_limit() {
    let cut = truncate_tooltip(&"č".repeat(300));
    assert_eq!(cut.encode_utf16().count(), TOOLTIP_MAX_UNITS);
    assert!(cut.ends_with('…'));
    assert_eq!(truncate_tooltip("short"), "short");
  }

  #[test]
  fn announced_changes_are_checked_at_once_but_at_most_every_gap() {
    let ms = Duration::from_millis;
    assert_eq!(rescan_in(false, None), None, "nothing announced: the 1 s check only");
    assert_eq!(rescan_in(false, Some(ms(5))), None);
    assert_eq!(rescan_in(true, None), Some(Duration::ZERO), "the first one: now");
    assert_eq!(rescan_in(true, Some(MIC_RESCAN_GAP)), Some(Duration::ZERO));
    assert_eq!(rescan_in(true, Some(ms(500))), Some(Duration::ZERO));
    assert_eq!(rescan_in(true, Some(ms(3))), Some(MIC_RESCAN_GAP - ms(3)), "a burst: after the gap");
  }
}
