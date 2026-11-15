//! Notification-area icon, owned by the background service (it runs whenever the app does).
//! Closing the window therefore "hides to tray"; the icon reopens it or exits everything.
//! Setup problems (`health`) put a badge on the icon and list themselves, with fixes, at the
//! top of the menu.

use crate::health::{Fix, Issue};
use log::{info, warn};
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
  DispatchMessageW, FindWindowW, GetForegroundWindow, IsIconic, MSG, PM_REMOVE, PeekMessageW, PostMessageW, SW_RESTORE,
  SW_SHOW, SW_SHOWNORMAL, SetForegroundWindow, ShowWindow, TranslateMessage, WM_CLOSE,
};
use windows::core::{HSTRING, PCWSTR, w};

/// Title of the main window; also how the service finds it (focus, close on exit).
pub const WINDOW_TITLE: &str = "GamersEqualizer - Audio Control Center";
/// At logon the service can start before Explorer's notification area exists; retry until it does.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CREATE_NO_WINDOW: u32 = 0x08000000;
/// How often the icon checks whether an app records from a microphone (a few registry reads).
const MIC_CHECK_INTERVAL: Duration = Duration::from_secs(1);
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
  /// Setup problems: badge, tooltip lines and the top of the menu.
  issues: Vec<Issue>,
}

impl TrayManager {
  pub fn new() -> Result<Self, String> {
    let tray_icon = TrayIconBuilder::new()
      // Left click opens the window; the menu is on right click.
      .with_menu_on_left_click(false)
      .with_tooltip(TOOLTIP)
      .with_icon(tray_icon(false, false)?)
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
  fn show_state(&self, mic_active: bool, issues: &[Issue]) {
    match tray_icon(mic_active, !issues.is_empty()) {
      Ok(icon) => {
        if let Err(e) = self.tray_icon.set_icon(Some(icon)) {
          warn!("[Tray] could not change the icon: {e}");
        }
      }
      Err(e) => warn!("[Tray] {e}"),
    }
    let base = if mic_active { TOOLTIP_MIC } else { TOOLTIP };
    let lines: Vec<String> =
      std::iter::once(base.to_string()).chain(issues.iter().map(|i| format!("⚠ {}", i.title()))).collect();
    let _ = self.tray_icon.set_tooltip(Some(truncate_tooltip(&lines.join("\n"))));
  }

  /// Principle 1: Air-gapped update check via OS default browser
  pub fn open_updates_browser() {
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
  pub fn new() -> Self {
    Self { tray: None, last_attempt: None, mic_active: false, mic_checked_at: None, issues: Vec::new() }
  }

  /// New list of setup problems (from `HealthMonitor::tick`).
  pub fn set_issues(&mut self, issues: Vec<Issue>) {
    self.issues = issues;
    if let Some(tray) = self.tray.as_mut() {
      if let Err(e) = tray.set_menu(&self.issues) {
        warn!("[Tray] {e}");
      }
      tray.show_state(self.mic_active, &self.issues);
    }
  }

  /// One service-loop step: pumps this thread's window messages (the tray icon's hidden
  /// window lives on it), creates the icon if missing, and handles clicks and menu items.
  pub fn poll(&mut self) -> Option<TrayAction> {
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
          if !self.issues.is_empty() {
            if let Err(e) = t.set_menu(&self.issues) {
              warn!("[Tray] {e}");
            }
            t.show_state(false, &self.issues);
          }
          self.tray = Some(t);
        }
        // Warn once, then keep retrying quietly; the service works without the icon.
        Err(e) if first => warn!("[Tray] {e}; retrying every {}s", RETRY_INTERVAL.as_secs()),
        Err(_) => {}
      }
    }
    let tray = self.tray.as_ref()?;

    if self.mic_checked_at.is_none_or(|t| t.elapsed() >= MIC_CHECK_INTERVAL) {
      self.mic_checked_at = Some(Instant::now());
      let active = crate::mic_usage::microphone_in_use();
      if active != self.mic_active {
        self.mic_active = active;
        info!("[Tray] microphone {}", if active { "in use" } else { "free" });
        tray.show_state(active, &self.issues);
      }
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
}

impl Default for TrayHost {
  fn default() -> Self {
    Self::new()
  }
}

/// The open main window, if any.
pub fn find_window() -> Option<HWND> {
  unsafe { FindWindowW(PCWSTR::null(), &HSTRING::from(WINDOW_TITLE)) }.ok().filter(|h| !h.is_invalid())
}

/// The main window is open and the active window (its own bar shows problems; no notification).
pub fn window_in_front() -> bool {
  find_window().is_some_and(|h| unsafe { GetForegroundWindow() } == h)
}

/// Brings an existing window to the front (restoring it if minimized). Returns false if none is open.
pub fn focus_window() -> bool {
  let Some(hwnd) = find_window() else { return false };
  unsafe {
    let _ = ShowWindow(hwnd, if IsIconic(hwnd).as_bool() { SW_RESTORE } else { SW_SHOW });
    let _ = SetForegroundWindow(hwnd);
  }
  true
}

/// Asks the window to close normally (it saves its geometry on the way out).
pub fn close_window() {
  if let Some(hwnd) = find_window() {
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
  unsafe {
    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
      let _ = TranslateMessage(&msg);
      DispatchMessageW(&msg);
    }
  }
}

fn tray_icon(mic_active: bool, warning: bool) -> Result<Icon, String> {
  Icon::from_rgba(crate::app_icon::rgba(TRAY_ICON_SIZE, mic_active, warning), TRAY_ICON_SIZE, TRAY_ICON_SIZE)
    .map_err(|e| format!("Icon creation failed: {}", e))
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
}
