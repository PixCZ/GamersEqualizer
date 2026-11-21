//! The tray flyout: a left click on the tray icon opens the volumes of the default output and of
//! every app with an audio session, like the Windows 11 volume flyout or EarTrumpet.
//!
//! It lives in the service, next to the icon: nothing starts on a click (a new window process
//! takes ~0.4 s before its first frame). A plain Win32 popup drawn by `paint`, made on the first
//! click and then only shown and hidden; hidden it has no timer and draws nothing. While open,
//! `tick` re-reads the list once a second.
//!
//! It shows what Windows has, as its mixer and EarTrumpet do: the endpoint volume of the output and
//! each app's session volume and mute. The device slider sets the endpoint volume. An app's
//! changes go through `take_changes` to the mixer, which owns the sessions: it puts them at the
//! dragged volume and stores the app volume that keeps them there
//! (`ChatMixController::set_app_heard_volume`); mute is the sessions' own Windows mute. A click on
//! the output's name lists the active outputs; a pick goes the same way to
//! `WinAudioSystem::select_output`, what the window's device list does.
//!
//! Along the edge nearest the tray icon a row opens the main window (what the menu's "Open
//! settings" does). App icons are read once per exe, after the first frame, and kept while the
//! service runs (an exe that gave none is asked again at the next opening).

mod paint;

pub(crate) use paint::glyph_rgba;

use crate::i18n::tr;
use crate::ui::prefs::{ThemePref, UiPrefs};
use crate::ui::theme::{Palette, palette_for};
use crate::win_audio::AudioEndpointInfo;
use crate::win_audio::chatmix::{ChatMixController, get_endpoint_volume, set_endpoint_mute, set_endpoint_volume};
use crate::win_audio::endpoint_lock::{active_devices, get_endpoint_info};
use crate::win_audio::mmdevice;
use log::{debug, info, warn};
use paint::Layout;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
  DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
  BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
  PAINTSTRUCT, ScreenToClient, UpdateWindow,
};
use windows::Win32::Media::Audio::{eConsole, eRender};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::HiDpi::{
  DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
  SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
  GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VIRTUAL_KEY, VK_DOWN, VK_END,
  VK_ESCAPE, VK_HOME, VK_LEFT, VK_M, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::Shell::SHDefExtractIconW;
use windows::Win32::UI::WindowsAndMessaging::{
  CreateWindowExW, DefWindowProcW, DestroyIcon, GetCursorPos, GetForegroundWindow, HICON, HWND_TOPMOST, IDC_ARROW,
  LoadCursorW, RegisterClassExW, SW_HIDE, SWP_SHOWWINDOW, SetForegroundWindow, SetWindowPos, ShowWindow, WA_INACTIVE,
  WM_ACTIVATE, WM_CAPTURECHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
  WM_PAINT, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{BOOL, HSTRING, PCWSTR, w};

/// While open: how often the list and the device volume are read again.
const REFRESH: Duration = Duration::from_secs(1);
/// After a change: when Windows' value is read back (the mixer applies it in the same loop pass).
const READ_BACK: Duration = Duration::from_millis(250);
/// How long the pointer rests on an app before its name shows.
const TIP_DELAY: Duration = Duration::from_millis(500);
/// A click on the icon while the flyout is open takes its focus first, which hides it: that same
/// click must not open it again.
const REOPEN_GUARD: Duration = Duration::from_millis(300);
/// One wheel notch or arrow key.
const STEP: f32 = 0.02;
/// Distance from the work area's edge (DIPs), as the Windows flyouts keep it.
const EDGE_MARGIN: i32 = 12;
const CLASS_NAME: PCWSTR = w!("GamersEqualizerFlyout");
const WHEEL_DELTA: f32 = 120.0;
/// winuser.h; the windows crate has it only behind the large `Win32_UI_Controls` feature.
const WM_MOUSELEAVE: u32 = 0x02A3;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target {
  /// An output by endpoint id: its Windows volume.
  Device(String),
  /// An app by exe name (case-insensitive): the volume and mute of its sessions.
  App(String),
  /// One of the mixer's channels, as the window's mixer has it.
  Channel(MixChannel),
}

/// The window mixer's channels besides Master (which is the output's row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixChannel {
  Game,
  Chat,
  Media,
  Mic,
}

/// A channel's level as the service has it; `volume` up to `max` (the mic's boost: above 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelLevel {
  pub volume: f32,
  pub muted: bool,
  pub max: f32,
}

/// A change for the service loop to hand to the mixer.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
  /// An app's sessions should play at `volume`; `save`: the end of a change.
  Volume {
    exe: String,
    volume: f32,
    save: bool,
  },
  Mute {
    exe: String,
    muted: bool,
  },
  /// The user picked this output (endpoint id).
  Output(String),
  /// A channel's whole state (`volume` up to its max); `save`: the end of a change.
  Channel {
    channel: MixChannel,
    volume: f32,
    muted: bool,
    save: bool,
  },
}

#[derive(Debug, Clone)]
pub(crate) struct Row {
  pub target: Target,
  pub name: String,
  /// Where the slider is, 0..1 of `max`.
  pub volume: f32,
  pub muted: bool,
  /// What the slider's end is (a channel's: the mic's boost); 1 = 100 %.
  pub max: f32,
  /// One process of the app, for its exe's icon.
  pub pid: u32,
  /// The exe's icon from `Flyout::icons` (which owns it); None: a glyph instead.
  pub icon: Option<HICON>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hit {
  Mute(usize),
  Slider(usize),
  /// The output's name: opens and closes the list of outputs.
  Name,
  /// An output in that list.
  Choice(usize),
  /// The "Open settings" row (the main window).
  Open,
}

/// Pointer and keyboard state the drawing shows.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Interaction {
  pub hover: Option<Hit>,
  pub drag: Option<usize>,
  /// One of `Layout::stops`; a row's is its `Hit::Slider`.
  pub focus: Option<Hit>,
  /// The row whose app name shows in a tip.
  pub tip: Option<usize>,
}

/// An exe's icon, destroyed with it.
struct AppIcon(HICON);

/// Where the taskbar is on the flyout's monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
  Bottom,
  Top,
  Left,
  Right,
}

struct Flyout {
  hwnd: HWND,
  rows: Vec<Row>,
  layout: Layout,
  ui: Interaction,
  palette: Palette,
  dark: bool,
  /// The icon (or the cursor) it opened at, for a new layout while open.
  anchor: RECT,
  visible: bool,
  /// The foreground window right after showing: when another one takes over, the flyout closes
  /// (also when Windows refused it the foreground, so no deactivation will come).
  foreground: HWND,
  hidden_at: Option<Instant>,
  refreshed_at: Instant,
  tracking_leave: bool,
  paint_failed: bool,
  /// Since when `ui.hover` is what it is (the name tip waits `TIP_DELAY`).
  hovered_at: Instant,
  /// Icons by lowercase exe name, at `icon_px`. An exe that gave none is not kept (its process may
  /// just have ended): the next opening asks again.
  icons: HashMap<String, AppIcon>,
  icon_px: i32,
  /// What the open list of outputs lists (empty while it is closed).
  outputs: Vec<AudioEndpointInfo>,
}

/// What a message handler leaves to do once the state is released (these calls send messages
/// back to the window, or to another one).
struct Reply {
  result: LRESULT,
  hide: bool,
  /// Open the main window (after hiding).
  open: bool,
}

impl Reply {
  fn done() -> Self {
    Self { result: LRESULT(0), hide: false, open: false }
  }

  fn hide() -> Self {
    Self { hide: true, ..Self::done() }
  }

  fn open() -> Self {
    Self { hide: true, open: true, ..Self::done() }
  }
}

/// Restores the thread's DPI awareness on drop.
struct PerMonitorDpi(DPI_AWARENESS_CONTEXT);

thread_local! {
  /// The service loop's thread owns the window; its messages come through `pump_messages`.
  static FLYOUT: RefCell<Option<Flyout>> = const { RefCell::new(None) };
  static CHANGES: RefCell<Vec<Change>> = const { RefCell::new(Vec::new()) };
  /// The mixer as the service loop last handed it (`set_mixer`).
  static MIXER: RefCell<Mixer> = const { RefCell::new(Mixer { master: None, channels: None }) };
}

/// The window mixer's Master output (the device behind a virtual default) and channel levels.
struct Mixer {
  master: Option<String>,
  channels: Option<[ChannelLevel; 4]>,
}

/// The left click on the tray icon: opens the flyout, or closes it when it is open.
/// `icon_rect`: the icon's position (asked for per-monitor aware); `prefs_dir`: the theme choice.
pub fn toggle(icon_rect: impl FnOnce() -> Option<tray_icon::Rect>, prefs_dir: &Path) -> Result<(), String> {
  let (visible, hidden_at, hwnd) = with(|f| (f.visible, f.hidden_at, Some(f.hwnd))).unwrap_or((false, None, None));
  if visible {
    if let Some(hwnd) = hwnd {
      with(Flyout::close);
      hide_window(hwnd);
    }
    return Ok(());
  }
  if hidden_at.is_some_and(|t| t.elapsed() < REOPEN_GUARD) {
    return Ok(());
  }
  let _dpi = PerMonitorDpi::enter();
  // Every opening starts with the list of outputs closed.
  with(|f| {
    f.outputs.clear();
    f.layout = f.layout.with_list(0, 0);
  });
  let anchor = icon_rect().map(icon_to_rect).or_else(cursor_rect).ok_or("no position for the flyout")?;
  show(anchor, dark_theme(prefs_dir))
}

/// Open now (the service loop then waits on window messages, not a plain sleep).
pub fn is_open() -> bool {
  with(|f| f.visible).unwrap_or(false)
}

/// From each service-loop pass: the output the window's Master slider sets (None: the Windows
/// default) and the channel levels (game, chat, media, mic; None: no channel rows, no shared
/// memory).
pub fn set_mixer(master: Option<&str>, channels: Option<[ChannelLevel; 4]>) {
  MIXER.with(|m| {
    if let Ok(mut m) = m.try_borrow_mut() {
      if m.master.as_deref() != master {
        m.master = master.map(str::to_string);
      }
      m.channels = channels;
    }
  });
}

/// The changes since the last call, oldest first.
pub fn take_changes() -> Vec<Change> {
  CHANGES.with(|q| q.try_borrow_mut().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default())
}

/// One service-loop pass: while open, close when another window took the foreground, and read
/// the list again every `REFRESH` (a new or gone app gets a new layout).
pub fn tick() {
  enum Next {
    Nothing,
    Hide(HWND),
    Relayout(RECT, bool),
  }
  let next = with(|f| {
    if !f.visible {
      return Next::Nothing;
    }
    // SAFETY: no arguments; the handle is only compared.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground != f.hwnd && foreground != f.foreground {
      f.close();
      return Next::Hide(f.hwnd);
    }
    // The name tip once the pointer rests on an app's row (a tooltip's delay).
    if let Some(Hit::Mute(i) | Hit::Slider(i)) = f.ui.hover
      && f.ui.drag.is_none()
      && f.ui.tip != Some(i)
      && f.hovered_at.elapsed() >= TIP_DELAY
    {
      f.ui.tip = Some(i);
      f.invalidate();
    }
    if f.ui.drag.is_some() || f.refreshed_at.elapsed() < REFRESH {
      return Next::Nothing;
    }
    f.refreshed_at = Instant::now();
    let mut rows = load_rows();
    let same = rows.len() == f.rows.len() && rows.iter().zip(&f.rows).all(|(a, b)| a.target == b.target);
    if !same {
      return Next::Relayout(f.anchor, f.dark);
    }
    // An output plugged in or out while its list is open: the panel changes, the window does not.
    if f.layout.list_open() {
      f.outputs = load_outputs();
      f.layout = f.layout.with_list(f.outputs.len(), f.layout.list_first());
    }
    f.attach_icons(&mut rows);
    f.rows = rows;
    f.invalidate();
    Next::Nothing
  })
  .unwrap_or(Next::Nothing);
  match next {
    Next::Nothing => {}
    Next::Hide(hwnd) => hide_window(hwnd),
    Next::Relayout(anchor, dark) => {
      let _dpi = PerMonitorDpi::enter();
      if let Err(e) = show(anchor, dark) {
        warn!("[Flyout] {e}");
      }
    }
  }
}

/// Lays the flyout out for `anchor` and shows it in front.
fn show(anchor: RECT, dark: bool) -> Result<(), String> {
  let started = Instant::now();
  let hwnd = match with(|f| f.hwnd) {
    Some(h) => h,
    None => create()?,
  };
  let mut rows = load_rows();
  let (work, monitor, dpi) = monitor_of(&anchor)?;
  let scale = dpi as f32 / 96.0;
  let margin = (EDGE_MARGIN as f32 * scale).round() as i32;
  let apps = rows.iter().filter(|r| matches!(r.target, Target::App(_))).count();
  let channels = rows.iter().filter(|r| matches!(r.target, Target::Channel(_))).count();
  let device = rows.first().is_some_and(|r| matches!(r.target, Target::Device(_)));
  let open_top = taskbar_edge(&anchor, &work, &monitor) == Edge::Top;
  // A new layout while the list is open (an app came or went) keeps it open where it was.
  let list_first = with(|f| f.layout.list_open().then_some(f.layout.list_first())).flatten();
  let outputs = if list_first.is_some() { load_outputs() } else { Vec::new() };
  let max_h = work.bottom - work.top - 2 * margin;
  let layout =
    Layout::new(scale, device, channels, apps, open_top, max_h).with_list(outputs.len(), list_first.unwrap_or(0));
  let (w, h) = (layout.width(), layout.height());
  let (x, y) = place(&anchor, &work, &monitor, (w, h), margin);
  let count = rows.len();
  with(|f| {
    if f.icon_px != layout.icon_px() {
      // Another DPI: icons at the new size.
      f.icons.clear();
      f.icon_px = layout.icon_px();
    }
    f.attach_icons(&mut rows);
    f.rows = rows;
    f.outputs = outputs;
    f.layout = layout;
    f.palette = palette_for(dark);
    f.dark = dark;
    f.anchor = anchor;
    // A new layout while open keeps a read-back that is due (`pick`).
    if !std::mem::replace(&mut f.visible, true) {
      f.refreshed_at = Instant::now();
    }
    f.tracking_leave = false;
    f.ui.hover = None;
    f.ui.drag = None;
  })
  .ok_or("the flyout state is busy")?;
  let dark_frame = BOOL::from(dark);
  // SAFETY: plain window calls on the flyout's own handle; `dark_frame` is a local read during the
  // call. A failure leaves the window hidden or behind, never invalid.
  unsafe {
    let _ = DwmSetWindowAttribute(
      hwnd,
      DWMWA_USE_IMMERSIVE_DARK_MODE,
      (&raw const dark_frame).cast(),
      size_of::<BOOL>() as u32,
    );
    SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW).map_err(|e| format!("SetWindowPos: {e}"))?;
    // The tray click lets this process take the foreground; then focus loss and Esc close it.
    let _ = SetForegroundWindow(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, false);
    // The first frame now, not on the next pass of the service loop.
    let _ = UpdateWindow(hwnd);
  }
  // SAFETY: no arguments; the handle is only stored for comparison.
  let foreground = unsafe { GetForegroundWindow() };
  with(|f| f.foreground = foreground);
  debug!("[Flyout] shown in {} ms ({count} rows, {dpi} dpi)", started.elapsed().as_millis());
  // Icons of apps not seen before: after the first frame, so opening never waits for them.
  with(Flyout::load_missing_icons);
  Ok(())
}

fn create() -> Result<HWND, String> {
  // SAFETY: the class and window use static strings and this module's window procedure; the
  // handle is owned by this thread's `FLYOUT` for the life of the service.
  let hwnd = unsafe {
    let instance = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {e}"))?;
    let class = WNDCLASSEXW {
      cbSize: size_of::<WNDCLASSEXW>() as u32,
      lpfnWndProc: Some(wndproc),
      hInstance: instance.into(),
      hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
      lpszClassName: CLASS_NAME,
      ..Default::default()
    };
    // 0 also when a failed earlier attempt registered it already; the window creation then tells.
    RegisterClassExW(&class);
    // Tool window: no taskbar button, no Alt+Tab entry.
    let hwnd = CreateWindowExW(
      WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
      CLASS_NAME,
      &HSTRING::from(tr("App volumes")),
      WS_POPUP,
      0,
      0,
      0,
      0,
      None,
      None,
      Some(instance.into()),
      None,
    )
    .map_err(|e| format!("CreateWindowExW: {e}"))?;
    // Windows 11: rounded corners with the system's border and shadow; Windows 10 ignores it.
    let round = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
      hwnd,
      DWMWA_WINDOW_CORNER_PREFERENCE,
      (&raw const round).cast(),
      size_of_val(&round) as u32,
    );
    hwnd
  };
  let stored = FLYOUT.with(|cell| {
    let mut slot = cell.try_borrow_mut().ok()?;
    *slot = Some(Flyout::new(hwnd));
    Some(())
  });
  stored.ok_or("the flyout state is busy")?;
  info!("[Flyout] window created");
  Ok(hwnd)
}

/// The default output first, the mixer's channels, then one row per app with a session (sorted by
/// name).
fn load_rows() -> Vec<Row> {
  let mut rows = Vec::new();
  let (master, channels) =
    MIXER.with(|m| m.try_borrow().map(|m| (m.master.clone(), m.channels)).unwrap_or((None, None)));
  if let Some((id, name)) = master_output(master.as_deref()) {
    let (volume, muted) = get_endpoint_volume(&id).unwrap_or((1.0, false));
    rows.push(Row { target: Target::Device(id), name, volume, muted, max: 1.0, pid: 0, icon: None });
  }
  rows.extend(channel_rows(channels));
  let sessions =
    ChatMixController::list_active_sessions().into_iter().map(|s| (s.exe_name, s.pid, s.volume, s.is_muted));
  rows.extend(app_rows(sessions));
  rows
}

/// One row per exe (an app often has several sessions and processes), sorted: the loudest of its
/// sessions, muted only when all of them are. Names that are no exe (a process Windows would not
/// name: "PID: 1234") are left out.
fn app_rows(sessions: impl Iterator<Item = (String, u32, f32, bool)>) -> Vec<Row> {
  let mut sessions: Vec<_> = sessions.filter(|(exe, ..)| exe.to_ascii_lowercase().ends_with(".exe")).collect();
  sessions.sort_by_key(|(exe, ..)| exe.to_ascii_lowercase());
  let mut rows: Vec<Row> = Vec::new();
  for (exe, pid, volume, muted) in sessions {
    let volume = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 1.0 };
    if let Some(last) = rows.last_mut()
      && matches!(&last.target, Target::App(e) if e.eq_ignore_ascii_case(&exe))
    {
      last.volume = last.volume.max(volume);
      last.muted &= muted;
      continue;
    }
    let name = exe.get(..exe.len().saturating_sub(4)).unwrap_or(&exe).to_string();
    rows.push(Row { target: Target::App(exe), name, volume, muted, max: 1.0, pid, icon: None });
  }
  rows
}

/// Game, chat, media, mic, the slider's end at each one's max.
fn channel_rows(levels: Option<[ChannelLevel; 4]>) -> Vec<Row> {
  let channels = [MixChannel::Game, MixChannel::Chat, MixChannel::Media, MixChannel::Mic];
  levels
    .into_iter()
    .flat_map(|levels| channels.into_iter().zip(levels))
    .map(|(channel, l)| {
      let max = if l.max.is_finite() && l.max > 0.0 { l.max } else { 1.0 };
      let volume = if l.volume.is_finite() { (l.volume / max).clamp(0.0, 1.0) } else { 1.0 };
      let name = channel.name().to_string();
      Row { target: Target::Channel(channel), name, volume, muted: l.muted, max, pid: 0, icon: None }
    })
    .collect()
}

impl MixChannel {
  pub(crate) fn name(self) -> &'static str {
    match self {
      Self::Game => tr("Game"),
      Self::Chat => tr("Chat"),
      Self::Media => tr("Media"),
      Self::Mic => tr("Mic"),
    }
  }

  /// The window mixer's icon and color for it.
  pub(crate) fn look(self) -> (crate::ui::theme::Icon, egui::Color32) {
    use crate::ui::tab_mixer::{CHAT_COLOR, GAME_COLOR, MEDIA_COLOR, MIC_COLOR};
    use crate::ui::theme::icons;
    match self {
      Self::Game => (icons::GAME, GAME_COLOR),
      Self::Chat => (icons::CHAT, CHAT_COLOR),
      Self::Media => (icons::MEDIA, MEDIA_COLOR),
      Self::Mic => (icons::MIC, MIC_COLOR),
    }
  }
}

/// The window's Master output (as the window's own slider; with a virtual default such as Sonar
/// the device behind it), else the Windows default.
fn master_output(master: Option<&str>) -> Option<(String, String)> {
  let device =
    master.and_then(|id| mmdevice::device(id).ok()).or_else(|| mmdevice::default_device(eRender, eConsole).ok())?;
  // SAFETY: a live device from the enumerator; `get_endpoint_info` frees the strings it reads.
  let info = unsafe { get_endpoint_info(&device, "") }.ok()?;
  Some((info.id, info.name))
}

/// The active outputs, as the window's output list has them.
fn load_outputs() -> Vec<AudioEndpointInfo> {
  match mmdevice::enumerator() {
    Ok(enumerator) => active_devices(&enumerator, eRender),
    Err(e) => {
      warn!("[Flyout] no list of outputs: {e}");
      Vec::new()
    }
  }
}

/// The change a pick of output `k` makes; None for the one in use or one no longer listed.
fn pick_output(outputs: &[AudioEndpointInfo], current: Option<&str>, k: usize) -> Option<Change> {
  outputs.get(k).filter(|o| Some(o.id.as_str()) != current).map(|o| Change::Output(o.id.clone()))
}

/// Top-left corner of a `size` flyout next to `anchor` (the icon), inside the monitor: along the
/// taskbar's edge, `margin` off the work area (an auto-hidden taskbar: off the icon).
fn place(anchor: &RECT, work: &RECT, monitor: &RECT, (w, h): (i32, i32), margin: i32) -> (i32, i32) {
  let edge = taskbar_edge(anchor, work, monitor);
  let center = ((anchor.left + anchor.right) / 2, (anchor.top + anchor.bottom) / 2);
  let along_x = (center.0 - w / 2).min(work.right - margin - w).max(work.left + margin);
  let along_y = (center.1 - h / 2).min(work.bottom - margin - h).max(work.top + margin);
  let (x, y) = match edge {
    Edge::Bottom => (along_x, work.bottom.min(anchor.top) - margin - h),
    Edge::Top => (along_x, work.top.max(anchor.bottom) + margin),
    Edge::Left => (work.left.max(anchor.right) + margin, along_y),
    Edge::Right => (work.right.min(anchor.left) - margin - w, along_y),
  };
  // Never off the monitor, whatever the taskbar does.
  (x.min(monitor.right - w).max(monitor.left), y.min(monitor.bottom - h).max(monitor.top))
}

/// The side the work area leaves free; with an auto-hidden taskbar the monitor edge nearest to
/// the icon.
fn taskbar_edge(anchor: &RECT, work: &RECT, monitor: &RECT) -> Edge {
  if work.bottom < monitor.bottom {
    return Edge::Bottom;
  }
  if work.top > monitor.top {
    return Edge::Top;
  }
  if work.left > monitor.left {
    return Edge::Left;
  }
  if work.right < monitor.right {
    return Edge::Right;
  }
  let (cx, cy) = ((anchor.left + anchor.right) / 2, (anchor.top + anchor.bottom) / 2);
  let gaps = [
    (monitor.bottom - cy, Edge::Bottom),
    (cy - monitor.top, Edge::Top),
    (cx - monitor.left, Edge::Left),
    (monitor.right - cx, Edge::Right),
  ];
  gaps.iter().min_by_key(|(gap, _)| *gap).map_or(Edge::Bottom, |(_, e)| *e)
}

/// (work area, monitor, DPI) of the monitor with `anchor` on it.
fn monitor_of(anchor: &RECT) -> Result<(RECT, RECT, u32), String> {
  let center = POINT { x: (anchor.left + anchor.right) / 2, y: (anchor.top + anchor.bottom) / 2 };
  let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
  let (mut dpi_x, mut dpi_y) = (96, 96);
  // SAFETY: `info` and the DPI values are locals the calls fill; the monitor handle needs no release.
  unsafe {
    let monitor = MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST);
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
      return Err("GetMonitorInfoW failed".into());
    }
    let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
  }
  Ok((info.rcWork, info.rcMonitor, dpi_x.max(48)))
}

fn icon_to_rect(r: tray_icon::Rect) -> RECT {
  let (left, top) = (r.position.x.round() as i32, r.position.y.round() as i32);
  RECT { left, top, right: left + r.size.width as i32, bottom: top + r.size.height as i32 }
}

fn cursor_rect() -> Option<RECT> {
  let mut p = POINT::default();
  // SAFETY: `p` is a local the call fills.
  unsafe { GetCursorPos(&mut p) }.ok()?;
  Some(RECT { left: p.x, top: p.y, right: p.x, bottom: p.y })
}

/// Dark unless the user picked light, or Windows is in light mode.
fn dark_theme(prefs_dir: &Path) -> bool {
  match UiPrefs::load_or_default(prefs_dir).theme {
    ThemePref::Dark => true,
    ThemePref::Light => false,
    ThemePref::System => !windows_light_theme(),
  }
}

fn windows_light_theme() -> bool {
  let (mut value, mut size) = (1u32, size_of::<u32>() as u32);
  // SAFETY: static NUL-terminated strings; `value` is a local DWORD the call fills, `size` its size.
  let read = unsafe {
    RegGetValueW(
      HKEY_CURRENT_USER,
      w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
      w!("AppsUseLightTheme"),
      RRF_RT_REG_DWORD,
      None,
      Some((&raw mut value).cast()),
      Some(&mut size),
    )
  };
  // Missing: Windows' default, light.
  read.is_err() || value != 0
}

fn hide_window(hwnd: HWND) {
  // SAFETY: the flyout's own handle; hiding an already hidden window does nothing.
  unsafe {
    let _ = ShowWindow(hwnd, SW_HIDE);
  }
}

/// Runs `f` on the flyout's state; None when there is none yet or it is in use (a message sent
/// back while a handler runs, which then gets Windows' default handling).
fn with<R>(f: impl FnOnce(&mut Flyout) -> R) -> Option<R> {
  FLYOUT.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
  let reply = with(|f| if f.hwnd == hwnd { f.on_message(msg, wparam, lparam) } else { None }).flatten();
  match reply {
    Some(Reply { result, hide, open }) => {
      if hide {
        hide_window(hwnd);
      }
      if open {
        crate::tray::show_or_launch_window();
      }
      result
    }
    // SAFETY: the arguments Windows passed to this window procedure, handed on unchanged.
    None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
  }
}

impl Flyout {
  fn new(hwnd: HWND) -> Self {
    Self {
      hwnd,
      rows: Vec::new(),
      layout: Layout::default(),
      ui: Interaction::default(),
      palette: palette_for(true),
      dark: true,
      anchor: RECT::default(),
      visible: false,
      foreground: HWND::default(),
      hidden_at: None,
      refreshed_at: Instant::now(),
      tracking_leave: false,
      paint_failed: false,
      hovered_at: Instant::now(),
      icons: HashMap::new(),
      icon_px: 0,
      outputs: Vec::new(),
    }
  }

  fn attach_icons(&self, rows: &mut [Row]) {
    for row in rows {
      if let Target::App(exe) = &row.target {
        row.icon = self.icons.get(&exe.to_ascii_lowercase()).map(|i| i.0);
      }
    }
  }

  fn load_missing_icons(&mut self) {
    let mut loaded = false;
    for row in &mut self.rows {
      let Target::App(exe) = &row.target else { continue };
      let key = exe.to_ascii_lowercase();
      if self.icons.contains_key(&key) {
        continue;
      }
      match AppIcon::load(row.pid, self.icon_px) {
        Ok(icon) => {
          row.icon = Some(icon.0);
          self.icons.insert(key, icon);
          loaded = true;
        }
        Err(e) => debug!("[Flyout] {exe}: no icon ({e})"),
      }
    }
    if loaded {
      self.invalidate();
    }
  }

  /// Marks it hidden (the caller hides the window); a drag in progress is saved.
  fn close(&mut self) {
    if let Some(i) = self.ui.drag.take() {
      self.save(i);
    }
    self.visible = false;
    self.hidden_at = Some(Instant::now());
  }

  fn on_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<Reply> {
    let done = Some(Reply::done());
    let (x, y) = (i32::from(lparam.0 as u16 as i16), i32::from((lparam.0 >> 16) as u16 as i16));
    match msg {
      WM_PAINT => {
        self.paint();
        done
      }
      // Everything is painted, nothing to erase (no flicker).
      WM_ERASEBKGND => Some(Reply { result: LRESULT(1), ..Reply::done() }),
      WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => {
        self.close();
        Some(Reply::hide())
      }
      WM_MOUSEMOVE => {
        self.track_leave();
        match self.ui.drag {
          Some(i) => {
            let volume = self.layout.value_at(i, x);
            // The pointer moved inside the same percent: nothing new to set.
            if self.rows.get(i).is_some_and(|r| r.muted || r.volume != volume) {
              self.set_volume(i, volume, false);
            }
          }
          None => self.set_hover(self.layout.hit(x, y)),
        }
        done
      }
      WM_MOUSELEAVE => {
        self.tracking_leave = false;
        self.set_hover(None);
        done
      }
      WM_LBUTTONDOWN => {
        self.ui.tip = None;
        let hit = self.layout.hit(x, y);
        // With the list open a click anywhere but on it only closes it.
        if self.layout.list_open() && !matches!(hit, Some(Hit::Choice(_))) {
          self.close_list();
          return done;
        }
        match hit {
          Some(Hit::Open) => {
            self.close();
            return Some(Reply::open());
          }
          Some(Hit::Name) => self.open_list(false),
          Some(Hit::Choice(k)) => self.pick(k),
          Some(Hit::Mute(i)) => {
            self.ui.focus = Some(Hit::Slider(i));
            self.toggle_mute(i);
          }
          Some(Hit::Slider(i)) => {
            self.ui.focus = Some(Hit::Slider(i));
            self.ui.drag = Some(i);
            // SAFETY: the flyout's own handle.
            unsafe { SetCapture(self.hwnd) };
            self.set_volume(i, self.layout.value_at(i, x), false);
          }
          None => {}
        }
        done
      }
      WM_LBUTTONUP => {
        if let Some(i) = self.ui.drag.take() {
          self.save(i);
          self.invalidate();
          // SAFETY: no arguments; releases this thread's capture.
          let _ = unsafe { ReleaseCapture() };
        }
        done
      }
      WM_CAPTURECHANGED => {
        if let Some(i) = self.ui.drag.take() {
          self.save(i);
          self.invalidate();
        }
        done
      }
      WM_MOUSEWHEEL => {
        let notches = f32::from((wparam.0 >> 16) as u16 as i16) / WHEEL_DELTA;
        let mut p = POINT { x, y };
        // SAFETY: the flyout's own handle and a local point (wheel positions are screen coordinates).
        let _ = unsafe { ScreenToClient(self.hwnd, &mut p) };
        if self.layout.in_list(p.x, p.y) {
          let first = self.layout.list_first() as f32 - notches.round();
          self.layout = self.layout.with_list(self.outputs.len(), first.max(0.0) as usize);
          self.set_hover(self.layout.hit(p.x, p.y));
          self.invalidate();
        } else if let Some(i) = self.layout.row_at(p.y) {
          self.step(i, notches * STEP);
        }
        done
      }
      WM_KEYDOWN => self.on_key(VIRTUAL_KEY(wparam.0 as u16)),
      _ => None,
    }
  }

  fn on_key(&mut self, key: VIRTUAL_KEY) -> Option<Reply> {
    let stops = self.layout.stops();
    // A stop gone with a new layout (the list closed) counts as none.
    let focus = self.ui.focus.filter(|f| stops.contains(f));
    let enter = key == VK_RETURN || key == VK_SPACE;
    match (key, focus) {
      (VK_ESCAPE, _) if self.layout.list_open() => {
        self.close_list();
        self.ui.focus = Some(Hit::Name);
      }
      (VK_ESCAPE, _) => {
        self.close();
        return Some(Reply::hide());
      }
      (_, Some(Hit::Open)) if enter => {
        self.close();
        return Some(Reply::open());
      }
      (_, Some(Hit::Name)) if enter => self.open_list(true),
      (_, Some(Hit::Choice(k))) if enter => self.pick(k),
      (VK_DOWN | VK_TAB | VK_UP, _) => {
        // SAFETY: reads this thread's key state as of the message being handled.
        let shift = unsafe { GetKeyState(i32::from(VK_SHIFT.0)) } < 0;
        self.ui.focus = next_stop(&stops, focus, key == VK_UP || (key == VK_TAB && shift));
        self.ui.tip = match self.ui.focus {
          Some(Hit::Slider(i)) => Some(i),
          _ => None,
        };
        if let Some(Hit::Choice(k)) = self.ui.focus {
          self.layout = self.layout.scrolled_to(k);
        }
      }
      (VK_RIGHT | VK_LEFT, Some(Hit::Slider(i))) => {
        let sign = if key == VK_RIGHT { 1.0 } else { -1.0 };
        self.step(i, sign * STEP);
      }
      (VK_HOME | VK_END, Some(Hit::Slider(i))) => self.set_volume(i, if key == VK_END { 1.0 } else { 0.0 }, true),
      (VK_SPACE | VK_M, Some(Hit::Slider(i))) => self.toggle_mute(i),
      _ => return None,
    }
    self.invalidate();
    Some(Reply::done())
  }

  /// Opens the list of outputs over the other groups (the window keeps its size); `keyboard`: the
  /// focus goes to the output in use.
  fn open_list(&mut self, keyboard: bool) {
    self.outputs = load_outputs();
    self.layout = self.layout.with_list(self.outputs.len(), 0);
    if keyboard {
      let current = self.current_output();
      let k = self.outputs.iter().position(|o| Some(o.id.as_str()) == current).unwrap_or(0);
      self.ui.focus = self.layout.list_open().then_some(Hit::Choice(k));
      self.layout = self.layout.scrolled_to(k);
    }
    self.ui.tip = None;
    self.ui.hover = None;
    self.invalidate();
  }

  fn close_list(&mut self) {
    self.outputs.clear();
    self.layout = self.layout.with_list(0, 0);
    if matches!(self.ui.focus, Some(Hit::Choice(_))) {
      self.ui.focus = Some(Hit::Name);
    }
    self.ui.hover = None;
    self.invalidate();
  }

  fn current_output(&self) -> Option<&str> {
    self.rows.first().and_then(|r| match &r.target {
      Target::Device(id) => Some(id.as_str()),
      Target::App(_) | Target::Channel(_) => None,
    })
  }

  /// Output `k` of the list picked: the service switches to it, the list closes.
  fn pick(&mut self, k: usize) {
    if let Some(change) = pick_output(&self.outputs, self.current_output(), k) {
      info!("[Flyout] output picked: {change:?}");
      push_change(change);
      // The new output's name, volume and sessions once the service switched.
      self.read_back_soon();
    }
    self.close_list();
  }

  fn step(&mut self, i: usize, delta: f32) {
    if let Some(row) = self.rows.get(i) {
      // A step is the same percent on every row (the mic's slider reaches 300 %).
      let v = ((row.volume * row.max + delta) * 100.0).round() / 100.0 / row.max;
      self.set_volume(i, v.clamp(0.0, 1.0), true);
    }
  }

  /// Moving a slider unmutes, as the Windows sliders do. `save`: the end of a change.
  fn set_volume(&mut self, i: usize, volume: f32, save: bool) {
    let Some(row) = self.rows.get_mut(i) else { return };
    let unmute = row.muted;
    row.volume = volume;
    row.muted = false;
    if unmute {
      apply_mute(row);
    }
    apply_volume(row, save);
    if save {
      self.read_back_soon();
    }
    self.invalidate();
  }

  fn toggle_mute(&mut self, i: usize) {
    let Some(row) = self.rows.get_mut(i) else { return };
    row.muted = !row.muted;
    apply_mute(row);
    self.read_back_soon();
    self.invalidate();
  }

  fn save(&mut self, i: usize) {
    if let Some(row) = self.rows.get(i)
      && !matches!(row.target, Target::Device(_))
    {
      apply_volume(row, true);
    }
    self.read_back_soon();
  }

  /// The next refresh in `READ_BACK`: the row then shows where Windows really put it (less than
  /// dragged when the app's channel is lower than that).
  fn read_back_soon(&mut self) {
    self.refreshed_at = Instant::now().checked_sub(REFRESH.saturating_sub(READ_BACK)).unwrap_or_else(Instant::now);
  }

  fn set_hover(&mut self, hit: Option<Hit>) {
    if self.ui.hover != hit {
      self.ui.hover = hit;
      self.ui.tip = None;
      self.hovered_at = Instant::now();
      self.invalidate();
    }
  }

  fn track_leave(&mut self) {
    if self.tracking_leave {
      return;
    }
    let mut tme = TRACKMOUSEEVENT {
      cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
      dwFlags: TME_LEAVE,
      hwndTrack: self.hwnd,
      dwHoverTime: 0,
    };
    // SAFETY: `tme` is a local naming the flyout's own window.
    self.tracking_leave = unsafe { TrackMouseEvent(&mut tme) }.is_ok();
  }

  fn invalidate(&self) {
    // SAFETY: the flyout's own handle; only queues a paint.
    unsafe {
      let _ = InvalidateRect(Some(self.hwnd), None, false);
    }
  }

  fn paint(&mut self) {
    let mut ps = PAINTSTRUCT::default();
    let apps = self.rows.iter().any(|r| matches!(r.target, Target::App(_)));
    let note = (!apps).then(|| tr("No app is playing sound"));
    // SAFETY: Begin/EndPaint pair on the flyout's own window inside its WM_PAINT; the DC is used
    // only between them.
    unsafe {
      let hdc = BeginPaint(self.hwnd, &mut ps);
      let painted = paint::paint(hdc, &self.layout, &self.rows, &self.outputs, self.ui, &self.palette, note);
      let _ = EndPaint(self.hwnd, &ps);
      // Once, not on every frame.
      if let Err(e) = painted
        && !std::mem::replace(&mut self.paint_failed, true)
      {
        warn!("[Flyout] drawing failed: {e}");
      }
    }
  }
}

/// The stop after `focus` (before it: `back`), around the ends; from none the first (last) one.
fn next_stop(stops: &[Hit], focus: Option<Hit>, back: bool) -> Option<Hit> {
  let n = stops.len();
  let at = focus.and_then(|f| stops.iter().position(|s| *s == f));
  let next = match (at, back) {
    (None, false) => 0,
    (None, true) => n.checked_sub(1)?,
    (Some(i), false) => (i + 1) % n,
    (Some(i), true) => (i + n - 1) % n,
  };
  stops.get(next).copied()
}

fn apply_volume(row: &Row, save: bool) {
  match &row.target {
    Target::Device(id) => set_endpoint_volume(id, row.volume),
    Target::App(exe) => push_change(Change::Volume { exe: exe.clone(), volume: row.volume, save }),
    Target::Channel(channel) => push_change(channel_change(*channel, row, save)),
  }
}

fn apply_mute(row: &Row) {
  match &row.target {
    Target::Device(id) => set_endpoint_mute(id, row.muted),
    Target::App(exe) => push_change(Change::Mute { exe: exe.clone(), muted: row.muted }),
    Target::Channel(channel) => push_change(channel_change(*channel, row, true)),
  }
}

fn channel_change(channel: MixChannel, row: &Row, save: bool) -> Change {
  // Whole percent, as the window's slider steps.
  let volume = (row.volume * row.max * 100.0).round() / 100.0;
  Change::Channel { channel, volume, muted: row.muted, save }
}

fn push_change(change: Change) {
  CHANGES.with(|q| {
    if let Ok(mut q) = q.try_borrow_mut() {
      queue_change(&mut q, change);
    }
  });
}

/// A drag sends a volume per mouse move: of a run for one app or channel only the last counts
/// (saved when any of them was).
fn queue_change(queue: &mut Vec<Change>, change: Change) {
  match (queue.last_mut(), change) {
    (Some(Change::Volume { exe: last, volume: v, save: s }), Change::Volume { exe, volume, save }) if *last == exe => {
      (*v, *s) = (volume, *s | save);
    }
    (
      Some(Change::Channel { channel: last, volume: v, muted: m, save: s }),
      Change::Channel { channel, volume, muted, save },
    ) if *last == channel => (*v, *m, *s) = (volume, muted, *s | save),
    (_, change) => queue.push(change),
  }
}

impl AppIcon {
  /// The icon of `pid`'s exe at `size` pixels.
  fn load(pid: u32, size: i32) -> Result<Self, String> {
    // A Win32 path: shell calls fail (E_FAIL) on the `\Device\...` form `get_exe_path_from_pid` gives.
    let path = crate::takeover::image_path(pid).ok_or("no exe path")?;
    Self::from_exe(&path.to_string_lossy(), size)
  }

  fn from_exe(path: &str, size: i32) -> Result<Self, String> {
    let mut icon = HICON::default();
    // SAFETY: a local path string and a local out handle; the icon is owned by the returned value.
    let hr =
      unsafe { SHDefExtractIconW(&HSTRING::from(path), 0, 0, Some(&mut icon), None, size.clamp(16, 256) as u32) };
    if hr.is_err() || icon.is_invalid() {
      return Err(format!("SHDefExtractIconW: {hr:?}"));
    }
    Ok(Self(icon))
  }
}

impl Drop for AppIcon {
  fn drop(&mut self) {
    // SAFETY: an icon this value owns, destroyed once.
    let _ = unsafe { DestroyIcon(self.0) };
  }
}

impl PerMonitorDpi {
  /// The service declares no DPI awareness: the flyout's window, its position and the icon's
  /// rectangle all use per-monitor physical pixels, sharp at any display scaling.
  fn enter() -> Self {
    // SAFETY: a system constant; returns the context to put back (null on failure, and setting
    // null back fails harmlessly).
    Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
  }
}

impl Drop for PerMonitorDpi {
  fn drop(&mut self) {
    // SAFETY: the context `enter` got back from Windows.
    unsafe {
      SetThreadDpiAwarenessContext(self.0);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const MONITOR: RECT = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
  const SIZE: (i32, i32) = (360, 300);

  /// The flyout as `paint` draws it, as 0x00RRGGBB pixels (top-down) of `layout`'s size.
  fn render(layout: &Layout, rows: &[Row], outputs: &[AudioEndpointInfo], ui: Interaction, dark: bool) -> Vec<u32> {
    use windows::Win32::Graphics::Gdi::{
      BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC,
      DeleteObject, GdiFlush, HGDIOBJ, SelectObject,
    };
    let (w, h) = (layout.width(), layout.height());
    let info = BITMAPINFO {
      bmiHeader: BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -h,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
      },
      ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    // SAFETY: a memory DC and DIB made and freed here; the pixels are read after `GdiFlush`.
    unsafe {
      let dc = CreateCompatibleDC(None);
      let bmp = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
      let old = SelectObject(dc, HGDIOBJ(bmp.0));
      paint::paint(dc, layout, rows, outputs, ui, &palette_for(dark), None).unwrap();
      let _ = GdiFlush();
      let px = std::slice::from_raw_parts(bits.cast::<u32>(), (w * h) as usize).iter().map(|p| p & 0xFF_FFFF).collect();
      SelectObject(dc, old);
      let _ = DeleteObject(HGDIOBJ(bmp.0));
      let _ = DeleteDC(dc);
      px
    }
  }

  fn app_row(exe: &str, icon: Option<HICON>, muted: bool) -> Row {
    Row { target: Target::App(exe.into()), name: exe.into(), volume: 0.6, muted, max: 1.0, pid: 0, icon }
  }

  fn device_row(id: &str, name: &str) -> Row {
    let target = Target::Device(id.into());
    Row { target, name: name.into(), volume: 0.5, muted: false, max: 1.0, pid: 0, icon: None }
  }

  /// Game 80 %, chat 100 %, media muted, mic 150 % of a 300 % slider.
  fn channels() -> [ChannelLevel; 4] {
    let l = |volume, muted, max| ChannelLevel { volume, muted, max };
    [l(0.8, false, 1.0), l(1.0, false, 1.0), l(0.5, true, 1.0), l(1.5, false, 3.0)]
  }

  fn outputs() -> Vec<AudioEndpointInfo> {
    let output = |id: &str, name: &str| AudioEndpointInfo { id: id.into(), name: name.into(), is_default: id == "b" };
    vec![output("a", "Speakers (Realtek)"), output("b", "Headphones (Arctis Nova)"), output("c", "LG TV (HDMI)")]
  }

  /// Pixels inside `r` that differ from the background (the top-left pixel).
  fn drawn(px: &[u32], width: i32, r: RECT) -> usize {
    let bg = px.first().copied().unwrap_or(0);
    (r.top..r.bottom)
      .flat_map(|y| (r.left..r.right).map(move |x| (y * width + x) as usize))
      .filter(|&i| px[i] != bg)
      .count()
  }

  #[test]
  fn an_app_icon_lands_in_its_button_with_its_colors() {
    // Windows' own exe, always there, with a colored icon.
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let layout = Layout::new(1.5, false, 0, 2, false, 10_000);
    let icon = AppIcon::from_exe(&format!(r"{windir}\explorer.exe"), layout.icon_px()).unwrap();
    let rows = [app_row("explorer.exe", Some(icon.0), false), app_row("none.exe", None, false)];
    let px = render(&layout, &rows, &[], Interaction::default(), true);
    let r = layout.icon_rect(0);
    let colored = (r.top..r.bottom)
      .flat_map(|y| (r.left..r.right).map(move |x| (y * layout.width() + x) as usize))
      .filter(|&i| {
        let p = px[i];
        let (r, g, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
        r.max(g).max(b) - r.min(g).min(b) > 40
      })
      .count();
    let area = ((r.right - r.left) * (r.bottom - r.top)) as usize;
    assert!(colored > area / 10, "the icon's colors are drawn: {colored} of {area} pixels");
    // The fallback: a drawn shape, not nothing.
    assert!(drawn(&px, layout.width(), layout.icon_rect(1)) > area / 10);
  }

  #[test]
  fn a_running_process_gets_its_exe_icon() {
    // Regression: the `\Device\HarddiskVolumeN\...` path made SHDefExtractIconW fail (E_FAIL), so
    // every app showed the fallback. cmd.exe has an icon and waits on its stdin.
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let mut child = std::process::Command::new(format!(r"{windir}\System32\cmd.exe"))
      .stdin(std::process::Stdio::piped())
      .stdout(std::process::Stdio::null())
      .spawn()
      .unwrap();
    let icon = AppIcon::load(child.id(), 24);
    let _ = child.kill();
    let _ = child.wait();
    assert!(icon.is_ok(), "{:?}", icon.err());
  }

  #[test]
  fn the_output_in_use_is_marked_and_a_pick_of_another_one_switches() {
    let (outputs, rows) = (outputs(), [device_row("b", "Headphones"), app_row("a.exe", None, false)]);
    // Room for all three over four app rows.
    let layout = Layout::new(1.5, true, 0, 4, false, 10_000).with_list(outputs.len(), 0);
    let px = render(&layout, &rows, &outputs, Interaction::default(), true);
    // The check mark sits in the first button's width of the current output only (on the panel's
    // fill, taken from the row's right end).
    let check = |k| {
      let r = layout.choice_rect(k);
      let fill = px[((r.top + 1) * layout.width() + r.right - 2) as usize];
      (r.top..r.bottom)
        .flat_map(|y| (r.left..r.left + layout.px(36)).map(move |x| (y * layout.width() + x) as usize))
        .filter(|&i| px[i] != fill)
        .count()
    };
    assert!(check(1) > 10 && check(0) == 0 && check(2) == 0, "{} {} {}", check(0), check(1), check(2));
    assert_eq!(pick_output(&outputs, Some("b"), 2), Some(Change::Output("c".into())));
    assert_eq!(pick_output(&outputs, Some("b"), 1), None, "already in use");
    assert_eq!(pick_output(&outputs, Some("b"), 3), None, "gone from the list");
    assert_eq!(pick_output(&outputs, None, 0), Some(Change::Output("a".into())));
  }

  #[test]
  fn tab_and_the_arrows_go_around_the_stops() {
    let stops = [Hit::Slider(1), Hit::Name, Hit::Open];
    assert_eq!(next_stop(&stops, None, false), Some(Hit::Slider(1)));
    assert_eq!(next_stop(&stops, None, true), Some(Hit::Open));
    assert_eq!(next_stop(&stops, Some(Hit::Open), false), Some(Hit::Slider(1)));
    assert_eq!(next_stop(&stops, Some(Hit::Slider(1)), true), Some(Hit::Open));
    assert_eq!(next_stop(&stops, Some(Hit::Choice(0)), false), Some(Hit::Slider(1)), "a stop gone: from the start");
    assert_eq!(next_stop(&[], None, true), None);
  }

  /// Writes the flyout as drawn for a few apps to `%TEMP%\flyout_render_*.bmp`, to look at.
  #[test]
  #[ignore = "report: writes a picture for a human"]
  fn report_render_flyout() {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let brave = format!(r"{local}\BraveSoftware\Brave-Browser\Application\brave.exe");
    let exe = |p: &str| AppIcon::from_exe(p, 36).ok();
    let icons =
      [exe(&brave), exe(r"C:\Program Files\Google\Chrome\Application\chrome.exe"), exe(r"C:\Windows\explorer.exe")];
    let names = ["brave.exe", "chrome.exe", "explorer.exe"];
    let mut rows = vec![device_row("b", "Headphones (Arctis Nova)")];
    rows.extend(channel_rows(Some(channels())));
    for (i, n) in names.iter().enumerate() {
      rows.push(app_row(n, icons.get(i).and_then(|i| i.as_ref()).map(|i| i.0), i == 2));
    }
    rows.push(app_row("noicon.exe", None, false));
    let outputs = outputs();
    for (name, dark, choices) in
      [("dark", true, 0), ("light", false, 0), ("dark_list", true, 3), ("light_list", false, 3)]
    {
      let layout = Layout::new(1.5, true, 4, 4, false, 10_000).with_list(choices, 0);
      let ui = Interaction { tip: Some(6), focus: (choices > 0).then_some(Hit::Choice(2)), ..Default::default() };
      let px = render(&layout, &rows, &outputs, ui, dark);
      let (w, h) = (layout.width(), layout.height());
      let mut bmp = Vec::new();
      let size = 54 + px.len() as u32 * 4;
      bmp.extend_from_slice(b"BM");
      for v in [size, 0, 54, 40] {
        bmp.extend_from_slice(&v.to_le_bytes());
      }
      bmp.extend_from_slice(&w.to_le_bytes());
      bmp.extend_from_slice(&(-h).to_le_bytes());
      bmp.extend_from_slice(&1u16.to_le_bytes());
      bmp.extend_from_slice(&32u16.to_le_bytes());
      bmp.extend_from_slice(&[0; 24]);
      for p in &px {
        bmp.extend_from_slice(&(p | 0xFF00_0000).to_le_bytes());
      }
      let path = std::env::temp_dir().join(format!("flyout_render_{name}.bmp"));
      std::fs::write(&path, bmp).unwrap();
      println!("{}", path.display());
    }
  }

  fn icon(x: i32, y: i32) -> RECT {
    RECT { left: x - 12, top: y - 12, right: x + 12, bottom: y + 12 }
  }

  #[test]
  fn the_flyout_sits_by_the_icon_on_every_taskbar_edge() {
    // Bottom taskbar (48 px): above it, centred on the icon but inside the right margin.
    let work = RECT { bottom: 1032, ..MONITOR };
    assert_eq!(place(&icon(1800, 1056), &work, &MONITOR, SIZE, 12), (1920 - 12 - 360, 1032 - 12 - 300));
    assert_eq!(place(&icon(900, 1056), &work, &MONITOR, SIZE, 12), (900 - 180, 720));
    // Top.
    let work = RECT { top: 48, ..MONITOR };
    assert_eq!(place(&icon(1800, 24), &work, &MONITOR, SIZE, 12), (1548, 60));
    // Left: right of it, centred on the icon's height but inside the bottom margin.
    let work = RECT { left: 48, ..MONITOR };
    assert_eq!(place(&icon(24, 1050), &work, &MONITOR, SIZE, 12), (60, 1080 - 12 - 300));
    // Right.
    let work = RECT { right: 1872, ..MONITOR };
    assert_eq!(place(&icon(1896, 500), &work, &MONITOR, SIZE, 12), (1872 - 12 - 360, 350));
  }

  #[test]
  fn an_auto_hidden_taskbar_or_a_second_monitor_still_put_it_by_the_icon() {
    // Work area = monitor: the taskbar shows over it, the flyout goes above the icon.
    assert_eq!(place(&icon(1800, 1056), &MONITOR, &MONITOR, SIZE, 12), (1548, 1044 - 12 - 300));
    // A monitor left of the main one (negative coordinates), taskbar at its bottom.
    let monitor = RECT { left: -2560, top: 0, right: 0, bottom: 1440 };
    let work = RECT { bottom: 1392, ..monitor };
    assert_eq!(place(&icon(-100, 1416), &work, &monitor, SIZE, 18), (-18 - 360, 1392 - 18 - 300));
    // Taller than the screen: kept on it.
    assert_eq!(place(&icon(900, 1056), &RECT { bottom: 1032, ..MONITOR }, &MONITOR, (360, 2000), 12).1, 0);
  }

  #[test]
  fn an_app_shows_its_loudest_session_and_is_muted_only_when_all_are() {
    let sessions = [
      ("Spotify.exe", 0.5, true),
      ("chrome.exe", 0.3, false),
      ("spotify.exe", 0.75, true),
      ("PID: 1234", 1.0, false),
      ("Chrome.exe", 0.6, true),
      ("Discord.exe", f32::NAN, false),
    ];
    let rows = app_rows(sessions.iter().map(|(e, v, m)| (e.to_string(), 1, *v, *m)));
    let got: Vec<_> = rows.iter().map(|r| (r.name.as_str(), r.volume, r.muted)).collect();
    assert_eq!(got, [("chrome", 0.6, false), ("Discord", 1.0, false), ("Spotify", 0.75, true)]);
  }

  #[test]
  fn a_drag_queues_only_its_last_volume_and_keeps_mutes_in_order() {
    let vol = |exe: &str, volume, save| Change::Volume { exe: exe.into(), volume, save };
    let mut queue = Vec::new();
    for change in [
      Change::Mute { exe: "a.exe".into(), muted: false },
      vol("a.exe", 0.2, false),
      vol("a.exe", 0.4, true),
      vol("a.exe", 0.5, false),
      vol("b.exe", 0.9, false),
      vol("a.exe", 0.1, false),
    ] {
      queue_change(&mut queue, change);
    }
    let want = [
      Change::Mute { exe: "a.exe".into(), muted: false },
      vol("a.exe", 0.5, true),
      vol("b.exe", 0.9, false),
      vol("a.exe", 0.1, false),
    ];
    assert_eq!(queue, want);
  }

  #[test]
  fn channel_rows_show_the_mixer_and_send_its_whole_state() {
    let rows = channel_rows(Some(channels()));
    let got: Vec<_> = rows.iter().map(|r| (r.target.clone(), r.volume, r.muted, r.max)).collect();
    let ch = |c| Target::Channel(c);
    assert_eq!(
      got,
      [
        (ch(MixChannel::Game), 0.8, false, 1.0),
        (ch(MixChannel::Chat), 1.0, false, 1.0),
        (ch(MixChannel::Media), 0.5, true, 1.0),
        (ch(MixChannel::Mic), 0.5, false, 3.0),
      ]
    );
    assert!(channel_rows(None).is_empty(), "no shared memory: no channel rows");
    // The slider's half of the mic's 300 % is 150 %; mute and volume travel together.
    let mic = rows.get(3).unwrap();
    let want = Change::Channel { channel: MixChannel::Mic, volume: 1.5, muted: false, save: true };
    assert_eq!(channel_change(MixChannel::Mic, mic, true), want);
    // A drag of one channel queues its last state; another channel stays its own change.
    let change = |channel, volume, muted, save| Change::Channel { channel, volume, muted, save };
    let mut queue = Vec::new();
    for c in [
      change(MixChannel::Game, 0.5, true, true),
      change(MixChannel::Game, 0.6, false, false),
      change(MixChannel::Chat, 0.3, false, false),
    ] {
      queue_change(&mut queue, c);
    }
    let want = [change(MixChannel::Game, 0.6, false, true), change(MixChannel::Chat, 0.3, false, false)];
    assert_eq!(queue, want);
  }
}
