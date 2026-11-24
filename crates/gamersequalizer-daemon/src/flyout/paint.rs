//! Layout and drawing of the tray flyout, rows as EarTrumpet has them: per app [icon = mute button,
//! slider, percent] with its name in a tip, the channels, then the output (its name, a list of the
//! outputs under it when open, [mute button, slider, percent]) and the "Open settings" row at the
//! edge nearest the tray icon. A top taskbar mirrors the order, so that row stays nearest the icon.
//! Drawn into a 32-bit bitmap: the shapes are anti-aliased here (GDI's own are not), text is GDI's
//! ClearType, icons are the apps' own, the bitmap goes to the window in one copy (no flicker).

use super::{Hit, Interaction, MixChannel, Row, Target, balance_value, loudness_db};
use crate::health::Fix;
use crate::i18n::{tr, trf};
use crate::ui::theme::{Palette, adapt_for, icons};
use crate::win_audio::AudioEndpointInfo;
use egui::Color32;
use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
  ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
  CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DRAW_TEXT_FORMAT,
  DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject,
  DrawTextW, FONT_QUALITY, FW_NORMAL, FW_SEMIBOLD, GdiFlush, GetTextExtentPoint32W, HDC, HFONT, HGDIOBJ,
  OUT_DEFAULT_PRECIS, SRCCOPY, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{DI_NORMAL, DrawIconEx};
use windows::core::{PCWSTR, w};

// Sizes in DIPs (96 DPI), scaled by `Layout::px`.
const WIDTH: i32 = 360;
const PAD: i32 = 12;
const NAME_H: i32 = 32;
const CONTROL_H: i32 = 40;
/// The app icons (the size `Layout::icon_px` asks Windows for).
const ICON: i32 = 24;
/// Between two groups: a clear gap with a thin line in its middle.
const GROUP_GAP: i32 = 17;
/// A group's caption ("Apps") over its rows.
const CAPTION_H: i32 = 22;
/// The "Open settings" row along the edge nearest the tray icon.
const OPEN_H: i32 = 40;
const TIP_H: i32 = 28;
const TIP_PAD: i32 = 8;
const NOTE_H: i32 = 40;
/// One output in the open list, and the list panel's inner margin.
const CHOICE_H: i32 = 32;
const LIST_PAD: i32 = 4;
/// Segoe MDL2 Assets ChevronDown / ChevronUp.
const CHEVRON_DOWN: char = '\u{E70D}';
const CHEVRON_UP: char = '\u{E70E}';
/// The setup problem next to the open row: its line of text, then "+N more" and the fix button.
const ISSUE_TEXT_H: i32 = 30;
const FIX_W: i32 = 132;
const FIX_H: i32 = 32;
const ISSUE_H: i32 = ISSUE_TEXT_H + FIX_H + 4;
/// The issue's second choice ("Keep ...", "Ignore"): a plain button on a line of its own under the
/// fix, wide enough for a device name.
const OTHER_FIX_H: i32 = FIX_H + 4;
const BUTTON: i32 = 36;
const GAP: i32 = 8;
const PERCENT_W: i32 = 44;
const TRACK_H: f32 = 4.0;
const THUMB_R: i32 = 10;
const RADIUS: f32 = 4.0;
const TEXT_PX: i32 = 14;
const CAPTION_PX: i32 = 12;
const GLYPH_PX: i32 = 16;
const SMALL_GLYPH_PX: i32 = 11;
/// Segoe MDL2 Assets: the same glyphs as Segoe Fluent Icons, and on Windows 10 too.
const GLYPH_FONT: PCWSTR = w!("Segoe MDL2 Assets");

/// Where everything sits, in pixels of the monitor the flyout is on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
  scale: f32,
  /// Row 0 is the output (its name on a line of its own over its slider).
  device: bool,
  /// Channel rows, after the output's in the row list.
  channels: usize,
  /// App rows shown, last in the row list (they are what is cut to fit the screen).
  apps: usize,
  /// A note in the apps group ("no app plays").
  note: bool,
  /// The open row is at the top (the flyout hangs below a top taskbar), else at the bottom.
  open_top: bool,
  /// The open list of outputs (0: closed): a panel over the groups beyond the output's, never
  /// a change of the flyout's size. It shows `list_rows` of them from `list_first` (it scrolls).
  outputs: usize,
  list_first: usize,
  list_rows: usize,
  /// Setup problems the tray lists (0: no block); the first is shown, the rest as "+N more".
  issues: usize,
  /// The first problem has a second choice (`Notice::other`): its button under the fix.
  other_fix: bool,
}

/// What the flyout writes besides the rows.
#[derive(Default)]
pub struct Texts<'a> {
  /// In the apps group ("no app plays").
  pub note: Option<&'a str>,
  pub issue: Option<&'a Notice>,
}

/// The first setup problem as the flyout shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
  pub short: &'static str,
  /// The tray menu's title, in the text's tip.
  pub title: String,
  pub button: &'static str,
  /// The tray menu's label of the fix, in the button's tip.
  pub label: String,
  pub fix: Fix,
  /// The tray menu's second item (label, fix), when the problem has one.
  pub other: Option<(String, Fix)>,
}

/// One horizontal band of the flyout, top to bottom (`Layout::slots`).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Slot {
  Pad,
  Open,
  /// The gap between two groups, its line in the middle.
  Line,
  Caption(Caption),
  Name,
  Row(usize),
  Note,
  Issue,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Caption {
  Channels,
  Apps,
}

impl Default for Layout {
  fn default() -> Self {
    Self::new(1.0, false, 0, 0, false, i32::MAX)
  }
}

impl Layout {
  /// The app rows cut to what fits into `max_height` pixels; the output, the channels and the
  /// open row always stay.
  pub fn new(scale: f32, device: bool, channels: usize, apps: usize, open_top: bool, max_height: i32) -> Self {
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    let mut layout = Self {
      scale,
      device,
      channels,
      apps,
      note: apps == 0,
      open_top,
      outputs: 0,
      list_first: 0,
      list_rows: 0,
      issues: 0,
      other_fix: false,
    };
    layout.cut_to(max_height);
    layout
  }

  /// With the block of `issues` setup problems (0: none), `other_fix`: the first has a second
  /// choice. The apps are cut again to `max_height`: the block is never cut. Before `with_list`
  /// (the list's room depends on it).
  pub fn with_issues(mut self, issues: usize, other_fix: bool, max_height: i32) -> Self {
    self.issues = issues;
    self.other_fix = other_fix && issues > 0;
    self.cut_to(max_height);
    self
  }

  fn cut_to(&mut self, max_height: i32) {
    while self.apps > 1 && self.height() > max_height {
      self.apps -= 1;
    }
  }

  pub fn issues(&self) -> usize {
    self.issues
  }

  /// With the list of `outputs` open (0: closed), scrolled to start at `first` as far as it can.
  pub fn with_list(mut self, outputs: usize, first: usize) -> Self {
    let (top, bottom) = self.list_room();
    let fit = (bottom - top - 2 * self.px(LIST_PAD)) / self.px(CHOICE_H).max(1);
    self.outputs = if self.device { outputs } else { 0 };
    self.list_rows = self.outputs.min(usize::try_from(fit).unwrap_or(0).max(1));
    self.list_first = first.min(self.outputs - self.list_rows);
    self
  }

  /// Scrolled so that output `k` shows.
  pub fn scrolled_to(self, k: usize) -> Self {
    let first = if k < self.list_first {
      k
    } else if k >= self.list_first + self.list_rows {
      k + 1 - self.list_rows
    } else {
      self.list_first
    };
    self.with_list(self.outputs, first)
  }

  pub fn list_first(&self) -> usize {
    self.list_first
  }

  pub fn list_open(&self) -> bool {
    self.list_rows > 0
  }

  /// Rows laid out: the output's, the channels', then the apps' shown.
  pub fn rows(&self) -> usize {
    usize::from(self.device) + self.channels + self.apps
  }

  /// The places Tab and the arrow keys go through, top to bottom as drawn; with the list open only
  /// it and the output's name (which closes it).
  pub fn stops(&self) -> Vec<Hit> {
    if self.list_rows > 0 {
      let list = (0..self.outputs).map(Hit::Choice);
      return if self.open_top {
        std::iter::once(Hit::Name).chain(list).collect()
      } else {
        list.chain(std::iter::once(Hit::Name)).collect()
      };
    }
    let stops = |slot| match slot {
      Slot::Open => vec![Hit::Open],
      Slot::Name => vec![Hit::Name],
      Slot::Row(i) => vec![Hit::Slider(i)],
      Slot::Issue => {
        let more = (self.issues > 1).then_some(Hit::More);
        more.into_iter().chain([Hit::Fix]).chain(self.other_fix.then_some(Hit::OtherFix)).collect()
      }
      _ => Vec::new(),
    };
    self.slots().into_iter().flat_map(|(slot, ..)| stops(slot)).collect()
  }

  /// The open list's panel holds the point.
  pub fn in_list(&self, x: i32, y: i32) -> bool {
    self.list_rows > 0 && contains(self.list_rect(), x, y)
  }

  pub fn px(&self, dip: i32) -> i32 {
    (dip as f32 * self.scale).round() as i32
  }

  pub fn width(&self) -> i32 {
    self.px(WIDTH)
  }

  pub fn height(&self) -> i32 {
    self.px(self.slots().last().map_or(0, |&(_, _, bottom)| bottom))
  }

  /// Pixels of the app icons.
  pub fn icon_px(&self) -> i32 {
    self.px(ICON)
  }

  /// The slider's track: (left end, right end, vertical center).
  pub fn track(&self, i: usize) -> (i32, i32, i32) {
    let b = self.button_rect(i);
    (
      self.px(PAD + BUTTON + GAP + THUMB_R),
      self.width() - self.px(PAD + PERCENT_W + GAP + THUMB_R),
      (b.top + b.bottom) / 2,
    )
  }

  /// The slider position under `x` for row `i`, in 1 % steps.
  pub fn value_at(&self, i: usize, x: i32) -> f32 {
    let (x0, x1, _) = self.track(i);
    let share = (x - x0) as f32 / (x1 - x0).max(1) as f32;
    (share.clamp(0.0, 1.0) * 100.0).round() / 100.0
  }

  /// The control under a point of the window.
  pub fn hit(&self, x: i32, y: i32) -> Option<Hit> {
    if self.in_list(x, y) {
      let shown = self.list_first..self.list_first + self.list_rows;
      return shown.into_iter().find(|&k| contains(self.choice_rect(k), x, y)).map(Hit::Choice);
    }
    let (slot, ..) = self.slot_at(y)?;
    let inside = (self.px(PAD)..self.width() - self.px(PAD)).contains(&x);
    // The open list is modal: outside it only the name (which closes it) answers.
    if self.list_rows > 0 && slot != Slot::Name {
      return None;
    }
    match slot {
      Slot::Open => Some(Hit::Open),
      Slot::Name if inside => Some(Hit::Name),
      Slot::Issue => [
        (self.fix_rect(), Hit::Fix),
        (self.other_fix_rect(), Hit::OtherFix),
        (self.more_rect(), Hit::More),
        (self.issue_line(), Hit::Issue),
      ]
      .into_iter()
      .find(|&(r, _)| contains(r, x, y))
      .map(|(_, hit)| hit),
      Slot::Row(i) => {
        let (x0, x1, _) = self.track(i);
        let b = self.button_rect(i);
        if (b.left..b.right).contains(&x) {
          Some(Hit::Mute(i))
        } else if (x0 - self.px(THUMB_R)..=x1 + self.px(THUMB_R)).contains(&x) {
          Some(Hit::Slider(i))
        } else {
          None
        }
      }
      _ => None,
    }
  }

  /// The row under a point (the output's name line included), for the mouse wheel.
  pub fn row_at(&self, y: i32) -> Option<usize> {
    if self.list_rows > 0 {
      return None;
    }
    match self.slot_at(y)?.0 {
      Slot::Row(i) => Some(i),
      Slot::Name => Some(0),
      _ => None,
    }
  }

  /// Every band top to bottom, with its top and bottom in DIPs.
  fn slots(&self) -> Vec<(Slot, i32, i32)> {
    let first_app = usize::from(self.device) + self.channels;
    let mut apps = vec![Slot::Caption(Caption::Apps)];
    apps.extend((first_app..first_app + self.apps).map(Slot::Row));
    if self.note {
      apps.push(Slot::Note);
    }
    let mut channels = Vec::new();
    if self.channels > 0 {
      channels.push(Slot::Caption(Caption::Channels));
      channels.extend((usize::from(self.device)..first_app).map(Slot::Row));
    }
    let mut device = Vec::new();
    if self.device {
      device.push(Slot::Name);
      device.push(Slot::Row(0));
    }
    let issue = if self.issues > 0 { vec![Slot::Issue] } else { Vec::new() };
    // A setup problem next to the open row, then the output, the apps farthest from the tray icon.
    let mut groups = [apps, channels, device, issue];
    if self.open_top {
      groups.reverse();
    }
    let mut order = vec![Slot::Pad];
    for group in groups.into_iter().filter(|g| !g.is_empty()) {
      if order.len() > 1 {
        order.push(Slot::Line);
      }
      order.extend(group);
    }
    order.push(Slot::Pad);
    if self.open_top {
      order.insert(0, Slot::Open);
    } else {
      order.push(Slot::Open);
    }
    let mut top = 0;
    order
      .into_iter()
      .map(|slot| {
        let height = match slot {
          Slot::Pad => PAD,
          Slot::Open => OPEN_H,
          Slot::Line => GROUP_GAP,
          Slot::Caption(_) => CAPTION_H,
          Slot::Name => NAME_H,
          Slot::Row(_) => CONTROL_H,
          Slot::Note => NOTE_H,
          Slot::Issue if self.other_fix => ISSUE_H + OTHER_FIX_H,
          Slot::Issue => ISSUE_H,
        };
        top += height;
        (slot, top - height, top)
      })
      .collect()
  }

  /// The band under `y` (pixels) with its top and bottom in pixels.
  fn slot_at(&self, y: i32) -> Option<(Slot, i32, i32)> {
    self
      .slots()
      .into_iter()
      .map(|(slot, top, bottom)| (slot, self.px(top), self.px(bottom)))
      .find(|&(_, top, bottom)| (top..bottom).contains(&y))
  }

  /// A band in pixels, inside the side padding (empty when it is not laid out).
  fn band(&self, slot: Slot) -> RECT {
    self.slots().into_iter().find(|&(s, ..)| s == slot).map_or_else(RECT::default, |(_, top, bottom)| RECT {
      left: self.px(PAD),
      top: self.px(top),
      right: self.width() - self.px(PAD),
      bottom: self.px(bottom),
    })
  }

  /// Vertical centers of the lines between the groups.
  fn lines(&self) -> Vec<i32> {
    let slots = self.slots().into_iter().filter(|&(s, ..)| s == Slot::Line);
    slots.map(|(_, top, bottom)| (self.px(top) + self.px(bottom)) / 2).collect()
  }

  fn captions(&self) -> Vec<(Caption, RECT)> {
    let captions = self.slots().into_iter().filter_map(|(s, ..)| if let Slot::Caption(c) = s { Some(c) } else { None });
    captions.map(|c| (c, self.band(Slot::Caption(c)))).collect()
  }

  /// The open row: the full width at the edge nearest the tray icon.
  fn open_rect(&self) -> RECT {
    RECT { left: 0, right: self.width(), ..self.band(Slot::Open) }
  }

  /// The output name line, clickable: it opens and closes the list of outputs.
  fn name_line(&self) -> RECT {
    self.band(Slot::Name)
  }

  /// The output's name text, room for the chevron right of it.
  fn name_rect(&self) -> RECT {
    let line = self.name_line();
    RECT { left: line.left + self.px(4), right: line.right - self.px(BUTTON), ..line }
  }

  fn chevron_rect(&self) -> RECT {
    let line = self.name_line();
    RECT { left: line.right - self.px(BUTTON), ..line }
  }

  /// Where the list's panel may go: beyond the output's group, away from the open row.
  fn list_room(&self) -> (i32, i32) {
    let gap = self.px(4);
    if self.open_top {
      (self.band(Slot::Row(0)).bottom + gap, self.height() - self.px(PAD))
    } else {
      (self.px(PAD), self.name_line().top - gap)
    }
  }

  /// The list's panel, anchored to the output's group.
  fn list_rect(&self) -> RECT {
    let (top, bottom) = self.list_room();
    let h = self.px(2 * LIST_PAD + self.list_rows as i32 * CHOICE_H);
    let top = if self.open_top { top } else { bottom - h };
    RECT { left: self.px(PAD), top, right: self.width() - self.px(PAD), bottom: top + h }
  }

  /// Output `k` in the open list (empty when it is scrolled out of view).
  pub(super) fn choice_rect(&self, k: usize) -> RECT {
    let Some(j) = k.checked_sub(self.list_first).filter(|&j| j < self.list_rows) else {
      return RECT::default();
    };
    let (panel, pad, j) = (self.list_rect(), self.px(LIST_PAD), j as i32);
    RECT {
      left: panel.left + pad,
      top: panel.top + self.px(LIST_PAD + j * CHOICE_H),
      right: panel.right - pad,
      bottom: panel.top + self.px(LIST_PAD + (j + 1) * CHOICE_H),
    }
  }

  fn button_rect(&self, i: usize) -> RECT {
    let band = self.band(Slot::Row(i));
    RECT { right: band.left + self.px(BUTTON), ..band }
  }

  pub(super) fn icon_rect(&self, i: usize) -> RECT {
    let b = self.button_rect(i);
    let s = self.icon_px();
    let (left, top) = ((b.left + b.right - s) / 2, (b.top + b.bottom - s) / 2);
    RECT { left, top, right: left + s, bottom: top + s }
  }

  /// The tip for a text `text_w` pixels wide at `b` (a row's icon, the issue's line): above it,
  /// or below it when there is no room above.
  fn tip_rect(&self, b: RECT, text_w: i32) -> RECT {
    let (w, h, gap) = (text_w + 2 * self.px(TIP_PAD), self.px(TIP_H), self.px(2));
    // Moved left as far as it needs to fit (the fix button's sits at the right edge).
    let w = w.min(self.width() - 2 * self.px(PAD));
    let left = b.left.min(self.width() - self.px(PAD) - w);
    let b = RECT { left, ..b };
    let top = if b.top - gap - h >= 0 { b.top - gap - h } else { b.bottom + gap };
    RECT { left: b.left, top, right: b.left + w, bottom: top + h }
  }

  fn percent_rect(&self, i: usize) -> RECT {
    RECT { left: self.width() - self.px(PAD + PERCENT_W), ..self.band(Slot::Row(i)) }
  }

  fn note_rect(&self) -> RECT {
    self.band(Slot::Note)
  }

  /// The issue's first line: the warning glyph and the short text (its tip: the full title).
  fn issue_line(&self) -> RECT {
    let band = self.band(Slot::Issue);
    RECT { bottom: band.top + self.px(ISSUE_TEXT_H), ..band }
  }

  fn issue_glyph_rect(&self) -> RECT {
    let line = self.issue_line();
    RECT { right: line.left + self.px(BUTTON), ..line }
  }

  fn issue_text_rect(&self) -> RECT {
    let line = self.issue_line();
    RECT { left: line.left + self.px(BUTTON + GAP), ..line }
  }

  /// The fix button, right under the text (empty without an issue).
  pub(super) fn fix_rect(&self) -> RECT {
    if self.issues == 0 {
      return RECT::default();
    }
    let line = self.issue_line();
    let top = line.bottom;
    RECT { left: line.right - self.px(FIX_W), top, right: line.right, bottom: top + self.px(FIX_H) }
  }

  /// The second choice under the fix button, from under the text to the right edge (empty without
  /// one).
  pub(super) fn other_fix_rect(&self) -> RECT {
    if !self.other_fix {
      return RECT::default();
    }
    let fix = self.fix_rect();
    let top = fix.bottom + self.px(4);
    RECT { left: self.issue_text_rect().left, top, right: fix.right, bottom: top + self.px(FIX_H) }
  }

  /// "+N more" left of the fix button (empty with one issue): it opens the main window.
  pub(super) fn more_rect(&self) -> RECT {
    if self.issues < 2 {
      return RECT::default();
    }
    let fix = self.fix_rect();
    RECT { left: self.issue_line().left + self.px(BUTTON), right: fix.left - self.px(GAP), ..fix }
  }
}

/// Draws the flyout onto `hdc` (the window's paint DC). `outputs`: the open list (row 0's
/// device is the one in use).
pub fn paint(
  hdc: HDC,
  layout: &Layout,
  rows: &[Row],
  outputs: &[AudioEndpointInfo],
  ui: Interaction,
  p: &Palette,
  texts: &Texts,
) -> Result<(), String> {
  let (w, h) = (layout.width(), layout.height());
  if w <= 0 || h <= 0 {
    return Ok(());
  }
  let info = dib_info(w, h);
  let mut bits = std::ptr::null_mut();
  // SAFETY: a memory DC, a DIB section and fonts made here and released below on every path; the
  // DIB's pixels are w * h u32 values GDI owns until `DeleteObject`, touched by the CPU only after
  // a `GdiFlush`. The icons are live handles the flyout's cache owns while it paints.
  unsafe {
    let mem = CreateCompatibleDC(Some(hdc));
    if mem.is_invalid() {
      return Err("CreateCompatibleDC failed".into());
    }
    let bitmap = match CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
      Ok(b) if !bits.is_null() => b,
      _ => {
        let _ = DeleteDC(mem);
        return Err("CreateDIBSection failed".into());
      }
    };
    let old = SelectObject(mem, HGDIOBJ(bitmap.0));
    let fonts = Fonts::new(layout);
    let old_font = SelectObject(mem, HGDIOBJ(fonts.regular.0));
    let pixels = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (w * h) as usize);
    draw_shapes(&mut Canvas { px: pixels, w, h }, layout, rows, ui, p);
    let _ = GdiFlush();
    draw_text(mem, layout, rows, p, texts, &fonts);
    for (i, row) in rows.iter().enumerate().take(layout.rows()) {
      if let Some(icon) = row.icon {
        let r = layout.icon_rect(i);
        let _ = DrawIconEx(mem, r.left, r.top, icon, r.right - r.left, r.bottom - r.top, 0, None, DI_NORMAL);
      }
    }
    let _ = GdiFlush();
    // Over the icons and the row above: the CPU's turn again.
    let mut canvas = Canvas { px: pixels, w, h };
    for (i, row) in rows.iter().enumerate().take(layout.rows()) {
      if row.muted && row.icon.is_some() {
        let r = layout.icon_rect(i);
        let (x, y, s) = (r.left as f32, r.top as f32, (r.right - r.left) as f32);
        let bg = if ui.hover == Some(Hit::Mute(i)) { hover_color(p) } else { p.card };
        canvas.shape(x, y, s, s, bg, |_, _| 0.6);
        // Diagonal mute slash so mute is never shown by dimming alone (WCAG 1.4.1).
        let half = layout.scale.max(1.0);
        let pad = 3.0 * layout.scale;
        canvas.shape(x + pad, y + pad, s - 2.0 * pad, s - 2.0 * pad, p.text, |px, py| {
          let dist = ((px - x) - (py - y)).abs() * std::f32::consts::FRAC_1_SQRT_2;
          coverage(dist - half)
        });
      }
    }
    if layout.list_rows > 0 {
      draw_list(&mut canvas, layout, ui, p);
      let _ = GdiFlush();
      draw_list_text(mem, layout, rows, outputs, p, &fonts);
      let _ = GdiFlush();
    }
    let issue = texts.issue.filter(|_| layout.issues > 0);
    // An app's name at its icon, the issue's full title at its line, the menu's fix label at the
    // button (it says "administrator" where the fix asks).
    let tip = match ui.tip.filter(|_| layout.list_rows == 0) {
      Some(Hit::Mute(i) | Hit::Slider(i)) => rows
        .get(i)
        .filter(|r| i < layout.rows() && !matches!(r.target, Target::Device(_)))
        .map(|r| (layout.button_rect(i), r.name.as_str())),
      Some(Hit::Issue) => issue.map(|n| (layout.issue_line(), n.title.as_str())),
      Some(Hit::Fix) => issue.map(|n| (layout.fix_rect(), n.label.as_str())),
      // The full label: a long device name is cut on the button.
      Some(Hit::OtherFix) => {
        issue.and_then(|n| n.other.as_ref()).map(|(label, _)| (layout.other_fix_rect(), label.as_str()))
      }
      _ => None,
    };
    let tip = tip.map(|(at, s)| {
      SelectObject(mem, HGDIOBJ(fonts.regular.0));
      let wide: Vec<u16> = s.encode_utf16().collect();
      let mut size = SIZE::default();
      let _ = GetTextExtentPoint32W(mem, &wide, &mut size);
      (layout.tip_rect(at, size.cx), s)
    });
    if let Some((rect, _)) = tip {
      let b = (rect.left as f32, rect.top as f32, (rect.right - rect.left) as f32, (rect.bottom - rect.top) as f32);
      let radius = 4.0 * layout.scale;
      canvas.round_rect(b.0, b.1, b.2, b.3, radius, p.card_stroke);
      let s = layout.scale.max(1.0);
      canvas.round_rect(b.0 + s, b.1 + s, b.2 - 2.0 * s, b.3 - 2.0 * s, radius - s, mix(p.card, p.text, 0.06));
    }
    let _ = GdiFlush();
    if let Some((mut rect, name)) = tip {
      rect.left += layout.px(TIP_PAD);
      rect.right -= layout.px(TIP_PAD);
      text(mem, name, rect, fonts.regular, p.text, DT_LEFT);
    }
    let copied = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
    SelectObject(mem, old);
    SelectObject(mem, old_font);
    fonts.delete();
    let _ = DeleteObject(HGDIOBJ(bitmap.0));
    let _ = DeleteDC(mem);
    copied.map_err(|e| format!("BitBlt failed: {e}"))
  }
}

/// A Segoe MDL2 glyph as `size` x `size` RGBA pixels in `color`, its edges in the alpha (for a
/// menu item's icon).
pub fn glyph_rgba(glyph: char, size: i32, color: Color32) -> Result<Vec<u8>, String> {
  let size = size.clamp(8, 256);
  let mut bits = std::ptr::null_mut();
  // SAFETY: a memory DC, a DIB section and a font made here and released below on every path; the
  // DIB's size * size u32 pixels are read only after `GdiFlush`.
  unsafe {
    let mem = CreateCompatibleDC(None);
    if mem.is_invalid() {
      return Err("CreateCompatibleDC failed".into());
    }
    let bitmap = match CreateDIBSection(Some(mem), &dib_info(size, size), DIB_RGB_COLORS, &mut bits, None, 0) {
      Ok(b) if !bits.is_null() => b,
      _ => {
        let _ = DeleteDC(mem);
        return Err("CreateDIBSection failed".into());
      }
    };
    let old = SelectObject(mem, HGDIOBJ(bitmap.0));
    let pixels = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (size * size) as usize);
    pixels.fill(0);
    // Gray anti-aliasing, not ClearType: white on black is then the coverage.
    let font = font(size, FW_NORMAL.0, ANTIALIASED_QUALITY, GLYPH_FONT);
    let old_font = SelectObject(mem, HGDIOBJ(font.0));
    SetBkMode(mem, TRANSPARENT);
    let rect = RECT { left: 0, top: 0, right: size, bottom: size };
    text(mem, &glyph.to_string(), rect, font, Color32::WHITE, DT_CENTER);
    let _ = GdiFlush();
    let rgba = pixels.iter().flat_map(|p| [color.r(), color.g(), color.b(), ((p >> 8) & 0xFF) as u8]).collect();
    SelectObject(mem, old_font);
    SelectObject(mem, old);
    let _ = DeleteObject(HGDIOBJ(font.0));
    let _ = DeleteObject(HGDIOBJ(bitmap.0));
    let _ = DeleteDC(mem);
    Ok(rgba)
  }
}

fn dib_info(w: i32, h: i32) -> BITMAPINFO {
  BITMAPINFO {
    bmiHeader: BITMAPINFOHEADER {
      biSize: size_of::<BITMAPINFOHEADER>() as u32,
      biWidth: w,
      biHeight: -h, // top-down rows
      biPlanes: 1,
      biBitCount: 32,
      biCompression: BI_RGB.0,
      ..Default::default()
    },
    ..Default::default()
  }
}

fn draw_shapes(c: &mut Canvas, layout: &Layout, rows: &[Row], ui: Interaction, p: &Palette) {
  let px = |d: f32| d * layout.scale;
  c.fill(p.card);
  let line = px(1.0).max(1.0);
  let full_w = layout.width() as f32;
  for y in layout.lines() {
    c.round_rect(px(PAD as f32), y as f32, full_w - 2.0 * px(PAD as f32), line, 0.0, p.grid);
  }
  // The open row: a footer (header for a top taskbar) in a shade of its own, as Windows has it.
  let o = layout.open_rect();
  let (oy, oh) = (o.top as f32, (o.bottom - o.top) as f32);
  let open_bg = if ui.hover == Some(Hit::Open) { hover_color(p) } else { mix(p.card, p.text, 0.03) };
  c.round_rect(0.0, oy, full_w, oh, 0.0, open_bg);
  c.round_rect(0.0, if layout.open_top { oy + oh - line } else { oy }, full_w, line, 0.0, p.grid);
  if ui.focus == Some(Hit::Open) {
    let inset = px(3.0);
    c.round_stroke((inset, oy + inset, full_w - 2.0 * inset, oh - 2.0 * inset), px(RADIUS), px(2.0), p.text);
  }
  // The fix: an accent button as the window's (`theme::accent_button`); the second choice a plain
  // filled one; "+N more" a flat one.
  if layout.issues > 0 {
    let o = layout.other_fix_rect();
    let shade = if ui.hover == Some(Hit::OtherFix) { 0.14 } else { 0.07 };
    let (ow, oh) = ((o.right - o.left) as f32, (o.bottom - o.top) as f32);
    c.round_rect(o.left as f32, o.top as f32, ow, oh, px(RADIUS), mix(p.card, p.text, shade));
    if layout.other_fix && ui.focus == Some(Hit::OtherFix) {
      focus_ring(c, layout, o, p);
    }
    let f = layout.fix_rect();
    // Hover darkens in both themes: towards the card in the dark one, towards the text in the light
    // one (towards its light card the fill would drop under 4.5:1 with white text).
    let darker = if p.dark { p.card } else { p.text };
    let fill = if ui.hover == Some(Hit::Fix) { mix(p.accent_fill, darker, 0.15) } else { p.accent_fill };
    let (fw, fh) = ((f.right - f.left) as f32, (f.bottom - f.top) as f32);
    c.round_rect(f.left as f32, f.top as f32, fw, fh, px(RADIUS), fill);
    if ui.focus == Some(Hit::Fix) {
      focus_ring(c, layout, f, p);
    }
    button_state(c, layout, Hit::More, layout.more_rect(), ui, p);
  }
  let hover_bg = hover_color(p);
  // The output's name: a button.
  if layout.device {
    button_state(c, layout, Hit::Name, layout.name_line(), ui, p);
  }
  let track_bg = p.text_tertiary;
  let thumb_bg = if p.dark { mix(p.card, p.text, 0.16) } else { Color32::WHITE };
  for (i, row) in rows.iter().enumerate().take(layout.rows()) {
    let b = layout.button_rect(i);
    let (bw, bh) = ((b.right - b.left) as f32, (b.bottom - b.top) as f32);
    if ui.hover == Some(Hit::Mute(i)) && row.mutes() {
      c.round_rect(b.left as f32, b.top as f32, bw, bh, px(RADIUS), hover_bg);
    }
    // An exe without an icon we can read: a plain app tile, drawn (no font needed).
    if matches!(row.target, Target::App(_)) && row.icon.is_none() && !row.muted {
      let r = layout.icon_rect(i);
      let (x, y, s) = (r.left as f32 + px(2.0), r.top as f32 + px(2.0), (r.right - r.left) as f32 - px(4.0));
      c.round_stroke((x, y, s, s), px(4.0), px(1.5), p.text_secondary);
      c.round_rect(x, y + px(5.0), s, px(1.5), 0.0, p.text_secondary);
    }
    let (x0, x1, cy) = layout.track(i);
    let (x0, x1, cy) = (x0 as f32, x1 as f32, cy as f32);
    let xv = x0 + (x1 - x0) * row.volume.clamp(0.0, 1.0);
    let th = px(TRACK_H);
    let fill = match &row.target {
      _ if row.muted => p.text_tertiary,
      Target::Channel(MixChannel::Balance) => balance_look(row.volume).1.map_or(p.accent, |c| adapt_for(p.dark, c)),
      Target::Channel(channel) => adapt_for(p.dark, channel.look().1),
      _ => p.accent,
    };
    // The balance fills from its middle (even) towards the side it favors.
    let from = if row.target == Target::Channel(MixChannel::Balance) { (x0 + x1) / 2.0 } else { x0 };
    c.round_rect(x0, cy - th / 2.0, x1 - x0, th, th / 2.0, track_bg);
    c.round_rect(from.min(xv), cy - th / 2.0, (xv - from).abs(), th, th / 2.0, fill);
    let r = px(THUMB_R as f32);
    if !p.dark {
      c.circle(xv, cy, r + px(1.0), p.card_stroke);
    }
    c.circle(xv, cy, r, thumb_bg);
    let inner = match (ui.drag == Some(i), ui.hover == Some(Hit::Slider(i))) {
      (true, _) => 5.0,
      (false, true) => 7.0,
      _ => 6.0,
    };
    c.circle(xv, cy, px(inner), fill);
    if ui.focus == Some(Hit::Slider(i)) {
      focus_ring(c, layout, RECT { right: layout.width() - layout.px(PAD), ..b }, p);
    }
  }
}

/// The balance slider at `value` (0 = all game, 1 = all chat): "50/50" in the middle, else how far
/// the other side is lowered, in the color of the side that stays at 100 % (`get_multipliers`).
/// None: even.
fn balance_look(value: f32) -> (String, Option<Color32>) {
  use crate::ui::tab_mixer::{CHAT_COLOR, GAME_COLOR};
  let balance = balance_value(value);
  let percent = (balance.abs() * 100.0).round();
  if percent < 1.0 {
    ("50/50".to_string(), None)
  } else {
    (format!("{percent:.0} %"), Some(if balance < 0.0 { GAME_COLOR } else { CHAT_COLOR }))
  }
}

/// Hover background and focus ring of the button `hit` at `r`.
fn button_state(c: &mut Canvas, layout: &Layout, hit: Hit, r: RECT, ui: Interaction, p: &Palette) {
  if ui.hover == Some(hit) {
    let (w, h) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
    c.round_rect(r.left as f32, r.top as f32, w, h, RADIUS * layout.scale, hover_color(p));
  }
  if ui.focus == Some(hit) {
    focus_ring(c, layout, r, p);
  }
}

/// The open list's panel over the rows: a card with a thin border, its buttons, a scroll bar
/// when not all outputs fit.
fn draw_list(c: &mut Canvas, layout: &Layout, ui: Interaction, p: &Palette) {
  let r = layout.list_rect();
  let (x, y, w, h) = (r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32);
  let (radius, line) = (RADIUS * 2.0 * layout.scale, layout.scale.max(1.0));
  c.round_rect(x, y, w, h, radius, p.card_stroke);
  c.round_rect(x + line, y + line, w - 2.0 * line, h - 2.0 * line, radius - line, mix(p.card, p.text, 0.04));
  for k in layout.list_first..layout.list_first + layout.list_rows {
    button_state(c, layout, Hit::Choice(k), layout.choice_rect(k), ui, p);
  }
  if layout.outputs > layout.list_rows {
    let (inner, total) = (h - 4.0 * line, layout.outputs as f32);
    let thumb_h = inner * layout.list_rows as f32 / total;
    let thumb_y = y + 2.0 * line + inner * layout.list_first as f32 / total;
    let bar_w = 3.0 * layout.scale;
    c.round_rect(x + w - 2.0 * line - bar_w, thumb_y, bar_w, thumb_h, bar_w / 2.0, p.text_tertiary);
  }
}

/// The keyboard focus around `r`, just outside it.
fn focus_ring(c: &mut Canvas, layout: &Layout, r: RECT, p: &Palette) {
  let out = layout.px(2);
  let (l, t) = ((r.left - out) as f32, (r.top - out) as f32);
  let (w, h) = ((r.right - r.left + 2 * out) as f32, (r.bottom - r.top + 2 * out) as f32);
  c.round_stroke((l, t, w, h), (RADIUS + 2.0) * layout.scale, 2.0 * layout.scale, p.text);
}

/// Text after the shapes (GDI writes straight into the bitmap).
///
/// # Safety
/// `dc` is a live memory DC with the flyout's bitmap selected.
unsafe fn draw_text(dc: HDC, layout: &Layout, rows: &[Row], p: &Palette, texts: &Texts, fonts: &Fonts) {
  // SAFETY: `dc` and the fonts are live (the contract); the caller puts its own font back.
  unsafe {
    SetBkMode(dc, TRANSPARENT);
    for (caption, mut r) in layout.captions() {
      r.left += layout.px(4);
      let label = match caption {
        Caption::Channels => tr("Channels"),
        Caption::Apps => tr("Apps"),
      };
      text(dc, label, r, fonts.caption, p.text_secondary, DT_LEFT);
    }
    for (i, row) in rows.iter().enumerate().take(layout.rows()) {
      let mut dim = if row.muted { p.text_tertiary } else { p.text };
      let glyph = match &row.target {
        Target::Channel(channel) => {
          let (icon, color) = channel.look();
          if !row.muted {
            dim = adapt_for(p.dark, color);
          }
          Some(if row.muted { icons::MUTE.glyph } else { icon.glyph })
        }
        Target::Device(_) => {
          text(dc, &row.name, layout.name_rect(), fonts.semibold, p.text, DT_LEFT);
          // The list opens away from the open row: up over the rows above, or down.
          let chevron = if (layout.list_rows > 0) == layout.open_top { CHEVRON_DOWN } else { CHEVRON_UP };
          text(dc, &chevron.to_string(), layout.chevron_rect(), fonts.small_glyphs, p.text_secondary, DT_CENTER);
          Some(if row.muted { icons::MUTE.glyph } else { icons::VOLUME.glyph })
        }
        // The app's own icon is drawn after the text; without one, a tile (`draw_shapes`) or mute.
        Target::App(_) => (row.icon.is_none() && row.muted).then_some(icons::MUTE.glyph),
      };
      if let Some(glyph) = glyph {
        text(dc, &glyph.to_string(), layout.button_rect(i), fonts.glyphs, dim, DT_CENTER);
      }
      let percent = p.text_secondary;
      // Percent only: the mic's boost in dB would not fit.
      let (level, percent) = match row.target {
        Target::Channel(MixChannel::Balance) => {
          let (text, color) = balance_look(row.volume);
          (text, color.map_or(percent, |c| adapt_for(p.dark, c)))
        }
        Target::Channel(MixChannel::MicLoudness) => (format!("{:.0} dB", loudness_db(row.volume)), percent),
        _ => (format!("{:.0} %", row.volume * row.max * 100.0), percent),
      };
      text(dc, &level, layout.percent_rect(i), fonts.regular, percent, DT_RIGHT);
    }
    if let Some(note) = texts.note {
      text(dc, note, layout.note_rect(), fonts.regular, p.text_secondary, DT_CENTER);
    }
    if let Some(issue) = texts.issue.filter(|_| layout.issues > 0) {
      // The warning color, as the window's status lines and the tray icon's badge have it.
      let glyph = icons::WARNING.glyph.to_string();
      text(dc, &glyph, layout.issue_glyph_rect(), fonts.glyphs, p.warn, DT_CENTER);
      text(dc, issue.short, layout.issue_text_rect(), fonts.regular, p.text, DT_LEFT);
      text(dc, issue.button, layout.fix_rect(), fonts.button, p.on_accent, DT_CENTER);
      if let Some((label, _)) = issue.other.as_ref().filter(|_| layout.other_fix) {
        let mut r = layout.other_fix_rect();
        r.left += layout.px(GAP);
        r.right -= layout.px(GAP);
        text(dc, label, r, fonts.regular, p.text, DT_CENTER);
      }
      if layout.issues > 1 {
        let mut more = layout.more_rect();
        more.left += layout.px(GAP);
        let label = trf("+{} more", &[&(layout.issues - 1)]);
        text(dc, &label, more, fonts.caption, p.text_secondary, DT_LEFT);
      }
    }
    let mut open = layout.open_rect();
    open.left += layout.px(PAD);
    let gear = icons::SETTINGS.glyph.to_string();
    text(dc, &gear, RECT { right: open.left + layout.px(BUTTON), ..open }, fonts.glyphs, p.text, DT_CENTER);
    open.left += layout.px(BUTTON + GAP);
    text(dc, tr("Open settings"), open, fonts.regular, p.text, DT_LEFT);
  }
}

/// The open list's outputs, the one in use (row 0's device) checked.
///
/// # Safety
/// `dc` is a live memory DC with the flyout's bitmap selected.
unsafe fn draw_list_text(
  dc: HDC,
  layout: &Layout,
  rows: &[Row],
  outputs: &[AudioEndpointInfo],
  p: &Palette,
  fonts: &Fonts,
) {
  let current = rows.first().and_then(|r| match &r.target {
    Target::Device(id) => Some(id.as_str()),
    Target::App(_) | Target::Channel(_) => None,
  });
  let shown = outputs.iter().enumerate().skip(layout.list_first).take(layout.list_rows);
  for (k, output) in shown {
    let r = layout.choice_rect(k);
    let check = RECT { right: r.left + layout.px(BUTTON), ..r };
    let name = RECT { left: check.right, right: r.right - layout.px(8), ..r };
    // SAFETY: the contract.
    unsafe {
      if current == Some(output.id.as_str()) {
        text(dc, &icons::CHECK.glyph.to_string(), check, fonts.small_glyphs, p.accent, DT_CENTER);
      }
      text(dc, &output.name, name, fonts.regular, p.text, DT_LEFT);
    }
  }
}

/// One line of text in `r`.
///
/// # Safety
/// `dc` is a live DC; `font` a live font (left selected).
unsafe fn text(dc: HDC, s: &str, mut r: RECT, font: HFONT, color: Color32, align: DRAW_TEXT_FORMAT) {
  // SAFETY: the contract; `wide` is a local that outlives the call.
  unsafe {
    SelectObject(dc, HGDIOBJ(font.0));
    SetTextColor(dc, colorref(color));
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    DrawTextW(dc, &mut wide, &mut r, align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
  }
}

/// A font `height` pixels high; the caller deletes it.
fn font(height: i32, weight: u32, quality: FONT_QUALITY, face: PCWSTR) -> HFONT {
  // SAFETY: plain values and a static face name.
  unsafe {
    CreateFontW(
      -height,
      0,
      0,
      0,
      weight as i32,
      0,
      0,
      0,
      DEFAULT_CHARSET,
      OUT_DEFAULT_PRECIS,
      CLIP_DEFAULT_PRECIS,
      quality,
      DEFAULT_PITCH.0 as u32,
      face,
    )
  }
}

/// The fonts of one paint, made for its scale.
struct Fonts {
  regular: HFONT,
  /// The fix button's text: the face the window draws its buttons in (sturdier at 14 px).
  button: HFONT,
  semibold: HFONT,
  caption: HFONT,
  glyphs: HFONT,
  small_glyphs: HFONT,
}

impl Fonts {
  fn new(layout: &Layout) -> Self {
    let f = |pt: i32, weight: u32, face| font(layout.px(pt), weight, CLEARTYPE_QUALITY, face);
    // The same file the window loads (`ui/theme.rs`); without it (Windows 10) GDI would substitute
    // some other face, so Segoe UI stays.
    static VARIABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let variable = *VARIABLE.get_or_init(|| std::path::Path::new(r"C:\Windows\Fonts\SegUIVar.ttf").exists());
    Self {
      regular: f(TEXT_PX, FW_NORMAL.0, w!("Segoe UI")),
      button: f(TEXT_PX, FW_NORMAL.0, if variable { w!("Segoe UI Variable Text") } else { w!("Segoe UI") }),
      semibold: f(TEXT_PX, FW_SEMIBOLD.0, w!("Segoe UI")),
      caption: f(CAPTION_PX, FW_NORMAL.0, w!("Segoe UI")),
      glyphs: f(GLYPH_PX, FW_NORMAL.0, GLYPH_FONT),
      small_glyphs: f(SMALL_GLYPH_PX, FW_NORMAL.0, GLYPH_FONT),
    }
  }

  /// Only once no DC has them selected.
  fn delete(self) {
    for f in [self.regular, self.button, self.semibold, self.caption, self.glyphs, self.small_glyphs] {
      // SAFETY: fonts this struct made; deleting a null one does nothing.
      let _ = unsafe { DeleteObject(HGDIOBJ(f.0)) };
    }
  }
}

fn contains(r: RECT, x: i32, y: i32) -> bool {
  (r.left..r.right).contains(&x) && (r.top..r.bottom).contains(&y)
}

fn hover_color(p: &Palette) -> Color32 {
  mix(p.card, p.text, 0.08)
}

fn colorref(c: Color32) -> COLORREF {
  COLORREF(u32::from(c.r()) | u32::from(c.g()) << 8 | u32::from(c.b()) << 16)
}

/// `a` moved towards `b` by `t` (0..=1).
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
  let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
  Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// The bitmap's pixels (0x00RRGGBB, top-down) and anti-aliased shapes on them.
struct Canvas<'a> {
  px: &'a mut [u32],
  w: i32,
  h: i32,
}

impl Canvas<'_> {
  fn fill(&mut self, c: Color32) {
    self.px.fill(rgb(c));
  }

  fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Color32) {
    self.round_rect(cx - r, cy - r, 2.0 * r, 2.0 * r, r, c);
  }

  fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Color32) {
    self.shape(x, y, w, h, c, |px, py| coverage(round_rect_distance(px, py, x, y, w, h, r)));
  }

  /// The outline of a rounded rectangle, `width` thick, inside its bounds.
  fn round_stroke(&mut self, (x, y, w, h): (f32, f32, f32, f32), r: f32, width: f32, c: Color32) {
    self.shape(x, y, w, h, c, |px, py| {
      let outer = coverage(round_rect_distance(px, py, x, y, w, h, r));
      let (iw, ih) = (w - 2.0 * width, h - 2.0 * width);
      let inner = coverage(round_rect_distance(px, py, x + width, y + width, iw, ih, (r - width).max(0.0)));
      outer - inner
    });
  }

  /// Blends `c` over every pixel of the box by `cover(pixel center)`.
  fn shape(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color32, cover: impl Fn(f32, f32) -> f32) {
    if !(w > 0.0 && h > 0.0) {
      return;
    }
    let (x0, x1) = ((x.floor() as i32).max(0), ((x + w).ceil() as i32).min(self.w));
    let (y0, y1) = ((y.floor() as i32).max(0), ((y + h).ceil() as i32).min(self.h));
    for py in y0..y1 {
      for px in x0..x1 {
        let a = cover(px as f32 + 0.5, py as f32 + 0.5).clamp(0.0, 1.0);
        if a > 0.0
          && let Some(p) = self.px.get_mut((py * self.w + px) as usize)
        {
          *p = blend(*p, c, a);
        }
      }
    }
  }
}

/// Signed distance from a point to a rounded rectangle (negative inside).
fn round_rect_distance(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
  let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
  let qx = (px - (x + w / 2.0)).abs() - (w / 2.0 - r);
  let qy = (py - (y + h / 2.0)).abs() - (h / 2.0 - r);
  (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

/// The share of a pixel a shape edge at `distance` covers.
fn coverage(distance: f32) -> f32 {
  (0.5 - distance).clamp(0.0, 1.0)
}

fn rgb(c: Color32) -> u32 {
  u32::from(c.r()) << 16 | u32::from(c.g()) << 8 | u32::from(c.b())
}

fn blend(dst: u32, c: Color32, a: f32) -> u32 {
  let ch = |d: u32, s: u8| (f32::from(s) * a + (d & 0xFF) as f32 * (1.0 - a)).round() as u32;
  ch(dst >> 16, c.r()) << 16 | ch(dst >> 8, c.g()) << 8 | ch(dst, c.b())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Output, 2 channels, 3 apps (rows 0, 1-2, 3-5).
  fn full(open_top: bool, outputs: usize) -> Layout {
    Layout::new(1.5, true, 2, 3, open_top, 10_000).with_list(outputs, 0)
  }

  #[test]
  fn slider_and_buttons_are_where_the_hit_test_finds_them() {
    for open_top in [false, true] {
      let l = full(open_top, 0);
      let (x0, x1, cy) = l.track(4);
      assert_eq!(l.hit(x0, cy), Some(Hit::Slider(4)));
      assert_eq!(l.hit(x1, cy), Some(Hit::Slider(4)));
      let b = l.button_rect(5);
      assert_eq!(l.hit(b.left + 1, b.top + 1), Some(Hit::Mute(5)));
      // An app is one line: icon, slider, percent; the rows of a group touch, in list order.
      assert_eq!(b.bottom - b.top, l.px(CONTROL_H));
      assert_eq!(l.button_rect(4).bottom, b.top);
      let icon = l.icon_rect(5);
      assert!(icon.left >= b.left && icon.right <= b.right && icon.right - icon.left == l.icon_px());
      // The output's name line opens its list; the wheel works over the whole output row.
      let name = l.name_rect();
      assert_eq!(l.hit(x0, name.top + 1), Some(Hit::Name));
      assert_eq!(l.hit(1, name.top + 1), None, "outside the padding");
      assert_eq!(l.row_at(name.top + 1), Some(0));
      assert_eq!(l.value_at(4, x0 - 50), 0.0);
      assert_eq!(l.value_at(4, x1 + 50), 1.0);
      assert_eq!(l.value_at(4, (x0 + x1) / 2), 0.5);
    }
  }

  #[test]
  fn apps_channels_output_then_settings_toward_the_tray_icon() {
    // Bottom taskbar: apps on top, the open row at the bottom edge.
    let l = full(false, 0);
    let (app, channel, output) = (l.button_rect(3), l.button_rect(1), l.button_rect(0));
    assert!(app.bottom <= channel.top && channel.bottom <= l.name_line().top && output.bottom <= l.open_rect().top);
    assert_eq!(l.hit(10, l.height() - 1), Some(Hit::Open));
    assert_eq!(l.hit(l.width() - 1, l.height() - l.px(OPEN_H)), Some(Hit::Open));
    // Top taskbar: the same, mirrored, so the open row stays nearest the icon.
    let t = full(true, 0);
    assert_eq!(t.hit(10, 0), Some(Hit::Open));
    let (app, channel, output) = (t.button_rect(3), t.button_rect(1), t.button_rect(0));
    assert!(t.open_rect().bottom <= t.name_line().top && output.bottom <= channel.top && channel.bottom <= app.top);
    assert_eq!(t.height(), l.height());
    // Within a group the order stays: the name over the slider, the apps in list order.
    assert!(t.name_line().bottom <= output.top && t.button_rect(3).top < t.button_rect(4).top);
    // A line between each two groups, a caption over the channels and the apps.
    assert_eq!(l.lines().len(), 2);
    let captions: Vec<_> = l.captions().into_iter().map(|(c, _)| c).collect();
    assert_eq!(captions, [Caption::Apps, Caption::Channels]);
    // No output and no app: the apps' caption and note, above the open row.
    let apps = Layout::new(1.0, false, 0, 0, false, 10_000);
    assert!(apps.note_rect().bottom <= apps.open_rect().top && apps.lines().is_empty());
  }

  #[test]
  fn the_keys_go_through_the_controls_as_drawn() {
    let want = [0, 1, 2, 3, 4, 5].map(Hit::Slider);
    let (apps, channels) = (&want[3..], &want[1..3]);
    let mut bottom = [apps, channels].concat();
    bottom.extend([Hit::Name, Hit::Slider(0), Hit::Open]);
    assert_eq!(full(false, 0).stops(), bottom);
    let mut top = vec![Hit::Open, Hit::Name, Hit::Slider(0)];
    top.extend([channels, apps].concat());
    assert_eq!(full(true, 0).stops(), top);
    // The open list is modal: its outputs as drawn, and the name that closes it.
    assert_eq!(full(false, 2).stops(), [Hit::Choice(0), Hit::Choice(1), Hit::Name]);
    assert_eq!(full(true, 2).stops(), [Hit::Name, Hit::Choice(0), Hit::Choice(1)]);
  }

  #[test]
  fn the_open_list_of_outputs_lies_over_the_other_groups_and_nothing_moves() {
    for open_top in [false, true] {
      let (closed, open) = (full(open_top, 0), full(open_top, 3));
      assert_eq!(open.height(), closed.height(), "the window keeps its size");
      assert_eq!((open.name_line(), open.button_rect(4)), (closed.name_line(), closed.button_rect(4)));
      let panel = open.list_rect();
      assert!(panel.top >= 0 && panel.bottom <= open.height() && panel.right <= open.width());
      if open_top {
        assert!(panel.top >= open.button_rect(0).bottom, "below the output's slider");
      } else {
        assert!(panel.bottom <= open.name_line().top, "above the output's name");
      }
      let c = open.choice_rect(1);
      assert_eq!(open.hit(c.left + 5, c.top + 1), Some(Hit::Choice(1)));
      assert_eq!(open.row_at(c.top + 1), None, "no wheel over the list");
      // Outside the panel only the name answers (a click elsewhere only closes the list).
      let b = open.button_rect(0);
      assert_eq!(open.hit(b.left + 1, b.top + 1), None);
      let name = open.name_rect();
      assert_eq!(open.hit(name.left + 1, name.top + 1), Some(Hit::Name));
      // More outputs than room: cut, scrollable, the window still the same.
      let many = full(open_top, 50);
      assert!(many.list_rows < 50 && many.height() == closed.height());
      assert!(many.list_rect().top >= 0 && many.list_rect().bottom <= many.height());
      let end = many.scrolled_to(49);
      assert_eq!(end.list_first() + end.list_rows, 50);
      let c = end.choice_rect(49);
      assert_eq!(end.hit(c.left + 5, c.top + 1), Some(Hit::Choice(49)));
      assert_eq!(end.scrolled_to(0).list_first(), 0);
    }
  }

  #[test]
  fn the_name_tip_stays_inside_the_flyout() {
    let l = Layout::new(2.0, true, 0, 3, false, 10_000);
    let tip = l.tip_rect(l.button_rect(2), 5_000);
    assert!(tip.bottom <= l.button_rect(2).top && tip.top >= 0, "above the icon");
    assert!(tip.right <= l.width());
    // The fix button's tip at the right edge: moved left, still inside.
    let l = l.with_issues(1, false, 10_000);
    let tip = l.tip_rect(l.fix_rect(), 200);
    assert!(tip.left >= 0 && tip.right <= l.width() - l.px(PAD) && tip.bottom <= l.fix_rect().top);
    // Nothing above the first row but the top padding: below it.
    let l = Layout::new(1.0, false, 0, 2, true, 10_000);
    assert!(l.tip_rect(l.button_rect(1), 50).top >= 0);
    let first = l.button_rect(0);
    let tip = l.tip_rect(first, 50);
    assert!(tip.top >= 0 && (tip.bottom <= first.top || tip.top >= first.bottom));
  }

  #[test]
  fn a_short_screen_cuts_the_apps_and_keeps_the_rest() {
    let full = Layout::new(1.0, true, 4, 30, false, 10_000);
    assert_eq!(full.apps, 30);
    let cut = Layout::new(1.0, true, 4, 30, false, 900);
    assert!(cut.apps < 30 && cut.height() <= 900);
    let tiny = Layout::new(1.0, true, 4, 30, false, 10);
    assert_eq!((tiny.apps, tiny.channels, tiny.rows()), (1, 4, 6));
    // The open list fits what is left, inside the window.
    let list = tiny.with_list(60, 0);
    assert!(list.list_rows >= 1 && list.list_rows < 60 && list.list_rect().top >= 0);
    // The gaps and captions count: what is left fits, one more app would not.
    let more = Layout { apps: cut.apps + 1, ..cut };
    assert!(more.height() > 900);
  }

  #[test]
  fn a_setup_problem_sits_next_to_the_open_row_and_its_buttons_answer() {
    for open_top in [false, true] {
      let none = full(open_top, 0);
      let one = Layout::new(1.5, true, 2, 3, open_top, 10_000).with_issues(1, false, 10_000);
      let two = one.with_issues(2, false, 10_000);
      assert_eq!(none.fix_rect(), RECT::default());
      assert_eq!(one.other_fix_rect(), RECT::default(), "no second choice: one button");
      assert!(!one.stops().contains(&Hit::OtherFix));
      assert_eq!(one.height(), none.height() + one.px(ISSUE_H + GROUP_GAP), "one band and its line");
      assert_eq!(one.lines().len(), 3);
      // Between the output's group and the open row, mirrored for a top taskbar.
      let (line, open, name, output) = (one.issue_line(), one.open_rect(), one.name_line(), one.button_rect(0));
      if open_top {
        assert!(open.bottom <= line.top && one.fix_rect().bottom <= name.top);
      } else {
        assert!(output.bottom <= line.top && one.fix_rect().bottom <= open.top);
      }
      // The button, the text (a tip, no action) and with more problems "+N more".
      let f = one.fix_rect();
      assert_eq!(one.hit(f.left + 2, f.top + 2), Some(Hit::Fix));
      assert_eq!(one.hit(f.right - 2, f.bottom - 2), Some(Hit::Fix));
      assert_eq!(one.hit(one.issue_text_rect().left + 2, line.top + 2), Some(Hit::Issue));
      assert_eq!(one.hit(f.left - 20, f.top + 2), None, "no \"+N more\" with one problem");
      let m = two.more_rect();
      assert!(m.right <= f.left && m.left > 0);
      assert_eq!(two.hit(m.left + 2, m.top + 2), Some(Hit::More));
      // Keys: in the order drawn, the block next to the open row.
      let stops = two.stops();
      let at = |h| stops.iter().position(|s| *s == h).unwrap();
      assert!(at(Hit::More) + 1 == at(Hit::Fix));
      if open_top {
        assert_eq!(stops.get(..3), Some(&[Hit::Open, Hit::More, Hit::Fix][..]));
      } else {
        assert_eq!(stops.get(stops.len() - 3..), Some(&[Hit::More, Hit::Fix, Hit::Open][..]));
      }
      assert_eq!(one.stops().iter().filter(|s| **s == Hit::More).count(), 0);
      // The open list keeps its room beyond the output's group, over the apps.
      let list = two.with_list(3, 0).list_rect();
      assert!(list.top >= 0 && list.bottom <= two.height());
      assert!(!contains(list, f.left + 2, f.top + 2), "the list never covers the fix");
    }
  }

  #[test]
  fn a_short_screen_cuts_apps_for_the_problem_block_never_the_block() {
    let plain = Layout::new(1.0, true, 4, 30, false, 900);
    let with = plain.with_issues(2, true, 900);
    assert!(with.issues() == 2 && with.apps < plain.apps && with.height() <= 900);
    let tiny = Layout::new(1.0, true, 4, 30, false, 10).with_issues(1, true, 10);
    assert_eq!((tiny.apps, tiny.issues()), (1, 1));
  }

  #[test]
  fn a_second_choice_gets_its_own_button_under_the_fix() {
    for open_top in [false, true] {
      let one = Layout::new(1.5, true, 2, 3, open_top, 10_000).with_issues(2, false, 10_000);
      let two = one.with_issues(2, true, 10_000);
      assert_eq!(two.height(), one.height() + two.px(OTHER_FIX_H), "one more line in the block");
      let (f, o) = (two.fix_rect(), two.other_fix_rect());
      // Under the fix, from under the text to the right edge: room for a device name.
      assert!(o.top >= f.bottom && o.left == two.issue_text_rect().left && o.right == f.right);
      assert!(o.right - o.left > f.right - f.left);
      assert_eq!(two.hit(o.left + 2, o.top + 2), Some(Hit::OtherFix));
      assert_eq!(two.hit(o.right - 2, o.bottom - 2), Some(Hit::OtherFix));
      assert_eq!(two.hit(f.left + 2, f.top + 2), Some(Hit::Fix));
      // Still inside the block, before the next band (the output's name or the open row).
      let next = if open_top { two.name_line().top } else { two.open_rect().top };
      assert!(o.bottom <= next);
      // Keys as drawn: "+N more", the fix, then the second choice.
      let stops = two.stops();
      let at = |h| stops.iter().position(|s| *s == h).unwrap();
      assert!(at(Hit::More) + 1 == at(Hit::Fix) && at(Hit::Fix) + 1 == at(Hit::OtherFix));
      // Its tip stays inside the flyout.
      let tip = two.tip_rect(o, 5_000);
      assert!(tip.left >= 0 && tip.right <= two.width() && tip.top >= 0);
      // Without a problem the flag does nothing.
      assert_eq!(full(open_top, 0).with_issues(0, true, 10_000).height(), full(open_top, 0).height());
    }
  }

  #[test]
  fn shapes_are_anti_aliased_and_stay_inside_the_bitmap() {
    let mut px = vec![0u32; 20 * 10];
    let mut c = Canvas { px: &mut px, w: 20, h: 10 };
    // Half outside on every side: no panic, only pixels inside change.
    c.circle(2.0, 2.0, 6.0, Color32::WHITE);
    c.round_rect(10.0, 2.5, 30.0, 4.0, 2.0, Color32::from_rgb(255, 0, 0));
    assert_eq!(px.first(), Some(&0xFF_FF_FF), "center of the circle fully covered");
    let edge = px.get(2 * 20 + 15).copied().unwrap() >> 16;
    assert!(edge > 0 && edge < 255, "half-covered edge pixel blends: {edge}");
    assert_eq!(px.get(9 * 20 + 19), Some(&0), "untouched");
  }

  #[test]
  fn a_menu_glyph_is_drawn_in_its_alpha() {
    let rgba = glyph_rgba(icons::SETTINGS.glyph, 16, Color32::from_rgb(10, 20, 30)).unwrap();
    assert_eq!(rgba.len(), 16 * 16 * 4);
    let alpha: Vec<u8> = rgba.chunks(4).map(|p| p[3]).collect();
    assert!(alpha.iter().filter(|&&a| a > 128).count() > 20, "the gear covers pixels");
    assert!(alpha.contains(&0) && rgba.chunks(4).all(|p| p[..3] == [10, 20, 30]));
  }
}
