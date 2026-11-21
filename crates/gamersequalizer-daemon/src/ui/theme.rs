//! Windows 11 (Fluent 2) look for egui: Segoe UI Variable + Segoe Fluent Icons from the
//! system, the Fluent type ramp, 32 px controls, rounded cards on a Mica-like background, a
//! light and a dark palette (following Windows unless the user picks one), and the few
//! custom widgets egui lacks (toggle switch, navigation item, info bar, page header).
//!
//! Fonts are read from C:\Windows\Fonts at startup; a missing file falls back to egui's
//! bundled fonts (and emoji instead of Fluent icons), so the app still runs on stripped
//! systems.

use egui::{
  Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame, Margin, Response, RichText,
  Sense, Shadow, Stroke, TextStyle, TextWrapMode, Theme, Ui, Vec2, Visuals, Widget, WidgetText,
};
use std::sync::atomic::{AtomicBool, Ordering};

const FONT_DIR: &str = r"C:\Windows\Fonts";
/// Fluent radii: controls, cards and flyouts.
pub const CONTROL_RADIUS: u8 = 5;
pub const CARD_RADIUS: u8 = 8;
pub const CARD_PADDING: i8 = 16;
/// Gap between cards on a page.
pub const CARD_GAP: f32 = 12.0;
/// Brand accent (orange), tuned per theme for contrast.
const ACCENT_DARK: Color32 = Color32::from_rgb(255, 140, 64);
const ACCENT_LIGHT: Color32 = Color32::from_rgb(196, 82, 0);
/// Fluent glyph offset below the text row's center, as a fraction of the text row height.
const ICON_DROP: f32 = 0.07;

static ICON_FONT: AtomicBool = AtomicBool::new(false);
static SEMIBOLD_FONT: AtomicBool = AtomicBool::new(false);

/// A Segoe Fluent Icons glyph with an emoji stand-in for systems without the font.
#[derive(Debug, Clone, Copy)]
pub struct Icon {
  pub glyph: char,
  pub emoji: &'static str,
}

pub mod icons {
  use super::Icon;
  pub const VOLUME: Icon = Icon { glyph: '\u{E767}', emoji: "🔊" };
  pub const MUTE: Icon = Icon { glyph: '\u{E74F}', emoji: "🔇" };
  pub const HEADPHONES: Icon = Icon { glyph: '\u{E7F6}', emoji: "🎧" };
  pub const SPEAKERS: Icon = Icon { glyph: '\u{E7F5}', emoji: "🔈" };
  pub const GAME: Icon = Icon { glyph: '\u{E7FC}', emoji: "🎮" };
  pub const CHAT: Icon = Icon { glyph: '\u{E8BD}', emoji: "💬" };
  pub const MEDIA: Icon = Icon { glyph: '\u{EC4F}', emoji: "🎵" };
  pub const MIC: Icon = Icon { glyph: '\u{E720}', emoji: "🎤" };
  pub const LOG: Icon = Icon { glyph: '\u{E9D9}', emoji: "📜" };
  pub const SETTINGS: Icon = Icon { glyph: '\u{E713}', emoji: "⚙" };
  pub const LOCK: Icon = Icon { glyph: '\u{E72E}', emoji: "🔒" };
  pub const UNLOCK: Icon = Icon { glyph: '\u{E785}', emoji: "🔓" };
  pub const POWER: Icon = Icon { glyph: '\u{E7E8}', emoji: "⏻" };
  pub const CLOSE: Icon = Icon { glyph: '\u{E8BB}', emoji: "✖" };
  pub const INFO: Icon = Icon { glyph: '\u{E946}', emoji: "ℹ" };
  pub const WARNING: Icon = Icon { glyph: '\u{E7BA}', emoji: "⚠" };
  pub const ERROR: Icon = Icon { glyph: '\u{EA39}', emoji: "⛔" };
  pub const CHECK: Icon = Icon { glyph: '\u{E73E}', emoji: "✔" };
  pub const GLOBE: Icon = Icon { glyph: '\u{E774}', emoji: "🌐" };
  pub const COLOR: Icon = Icon { glyph: '\u{E790}', emoji: "🎨" };
  pub const EQUALIZER: Icon = Icon { glyph: '\u{E9E9}', emoji: "🎚" };
  pub const DELETE: Icon = Icon { glyph: '\u{E74D}', emoji: "🗑" };
  pub const SAVE: Icon = Icon { glyph: '\u{E74E}', emoji: "💾" };
  pub const OPEN: Icon = Icon { glyph: '\u{E838}', emoji: "📂" };
}

/// Message severity of an info bar / status text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
  Info,
  Success,
  Warning,
  Error,
}

/// Semantic colors of the current theme (Fluent 2 tokens).
#[derive(Debug, Clone, Copy)]
pub struct Palette {
  pub card: Color32,
  pub card_stroke: Color32,
  /// Recessed area inside a card (lists, meters, graph background).
  pub inset: Color32,
  pub text: Color32,
  pub text_secondary: Color32,
  pub text_tertiary: Color32,
  pub accent: Color32,
  /// Text/icons on an accent fill.
  pub on_accent: Color32,
  pub ok: Color32,
  pub warn: Color32,
  pub err: Color32,
  pub grid: Color32,
  pub nav_hover: Color32,
  pub nav_selected: Color32,
  pub dark: bool,
}

const DARK: Palette = Palette {
  card: Color32::from_rgb(43, 43, 43),
  card_stroke: Color32::from_rgb(28, 28, 28),
  inset: Color32::from_rgb(32, 32, 32),
  text: Color32::from_rgb(255, 255, 255),
  text_secondary: Color32::from_rgb(200, 200, 200),
  text_tertiary: Color32::from_rgb(150, 150, 150),
  accent: ACCENT_DARK,
  on_accent: Color32::from_rgb(20, 20, 20),
  ok: Color32::from_rgb(108, 203, 95),
  warn: Color32::from_rgb(252, 225, 0),
  err: Color32::from_rgb(255, 153, 164),
  grid: Color32::from_rgb(52, 52, 52),
  nav_hover: Color32::from_rgb(45, 45, 45),
  nav_selected: Color32::from_rgb(50, 50, 50),
  dark: true,
};

const LIGHT: Palette = Palette {
  card: Color32::from_rgb(251, 251, 251),
  card_stroke: Color32::from_rgb(229, 229, 229),
  inset: Color32::from_rgb(243, 243, 243),
  text: Color32::from_rgb(27, 27, 27),
  text_secondary: Color32::from_rgb(96, 96, 96),
  text_tertiary: Color32::from_rgb(130, 130, 130),
  accent: ACCENT_LIGHT,
  on_accent: Color32::from_rgb(255, 255, 255),
  ok: Color32::from_rgb(15, 123, 15),
  warn: Color32::from_rgb(157, 93, 0),
  err: Color32::from_rgb(196, 43, 28),
  grid: Color32::from_rgb(225, 225, 225),
  nav_hover: Color32::from_rgb(234, 234, 234),
  nav_selected: Color32::from_rgb(230, 230, 230),
  dark: false,
};

/// The palette outside egui (the tray flyout draws itself).
pub fn palette_for(dark: bool) -> Palette {
  if dark { DARK } else { LIGHT }
}

// --- setup -------------------------------------------------------------------------------------

/// Fonts and both themes; call once with the egui context before the first frame.
pub fn install(ctx: &egui::Context) {
  ctx.set_fonts(fonts());
  ctx.set_style_of(Theme::Dark, style(&DARK));
  ctx.set_style_of(Theme::Light, style(&LIGHT));
}

pub fn palette(ui: &Ui) -> Palette {
  if ui.visuals().dark_mode { DARK } else { LIGHT }
}

/// A bright brand/channel color made readable on the light theme (darkened); unchanged on dark.
pub fn adapt(ui: &Ui, c: Color32) -> Color32 {
  adapt_for(ui.visuals().dark_mode, c)
}

/// `adapt` outside egui (the tray flyout).
pub fn adapt_for(dark: bool, c: Color32) -> Color32 {
  if dark {
    c
  } else {
    let k = |v: u8| (v as f32 * 0.62) as u8;
    Color32::from_rgb(k(c.r()), k(c.g()), k(c.b()))
  }
}

pub fn severity_color(ui: &Ui, s: Severity) -> Color32 {
  let p = palette(ui);
  match s {
    Severity::Info => p.text_secondary,
    Severity::Success => p.ok,
    Severity::Warning => p.warn,
    Severity::Error => p.err,
  }
}

// --- text helpers --------------------------------------------------------------------------

/// Icon glyph as text (Fluent icon font, or the emoji stand-in).
pub fn icon(icon: Icon, size: f32) -> RichText {
  if ICON_FONT.load(Ordering::Relaxed) {
    RichText::new(icon.glyph.to_string()).family(FontFamily::Name("icons".into())).size(size)
  } else {
    RichText::new(icon.emoji).size(size)
  }
}

/// Icon + label in one string for buttons and combo boxes (emoji fallback is inline text).
pub fn with_icon(icon: Icon, label: &str) -> String {
  // Fluent glyphs live in their own font family, which plain button text can't switch to;
  // the emoji font is part of the normal family, so inline labels use the emoji.
  format!("{}  {label}", icon.emoji)
}

/// Semibold text at `size` (Fluent "Body Strong" / "Subtitle" / "Title").
pub fn strong(text: impl Into<String>, size: f32) -> RichText {
  let t = RichText::new(text).size(size);
  if SEMIBOLD_FONT.load(Ordering::Relaxed) { t.family(FontFamily::Name("semibold".into())) } else { t.strong() }
}

/// Secondary caption text (12 px).
pub fn caption(text: impl Into<String>) -> RichText {
  RichText::new(text).small().weak()
}

/// Icon followed by single-line text, the glyph centered on the text's visual middle.
pub fn icon_label(ui: &mut Ui, ic: Icon, size: f32, color: Color32, text: impl Into<WidgetText>) -> Response {
  icon_text(ui, ic, size, color, text.into(), false)
}

/// `icon_label` with the icon after the text.
pub fn label_icon(ui: &mut Ui, text: impl Into<WidgetText>, ic: Icon, size: f32, color: Color32) -> Response {
  icon_text(ui, ic, size, color, text.into(), true)
}

// --- layout widgets ------------------------------------------------------------------------

/// Page title (Fluent "Title", 28 px) with an icon and an optional one-line description.
pub fn page_header(ui: &mut Ui, ic: Icon, color: Color32, title: &str, subtitle: Option<&str>) {
  ui.add_space(4.0);
  let c = adapt(ui, color);
  icon_label(ui, ic, 26.0, c, strong(title, 28.0));
  if let Some(s) = subtitle {
    ui.label(RichText::new(s).color(palette(ui).text_secondary));
  }
  ui.add_space(CARD_GAP);
}

/// Rounded card filling the width; the main grouping element of every page.
pub fn card<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
  let p = palette(ui);
  let r = Frame::NONE
    .fill(p.card)
    .stroke(Stroke::new(1.0_f32, p.card_stroke))
    .corner_radius(CornerRadius::same(CARD_RADIUS))
    .inner_margin(Margin::same(CARD_PADDING))
    .show(ui, |ui| {
      ui.set_width(ui.available_width());
      add_contents(ui)
    })
    .inner;
  ui.add_space(CARD_GAP);
  r
}

/// Recessed area inside a card (lists, meters).
pub fn inset_frame(ui: &Ui) -> Frame {
  Frame::NONE.fill(palette(ui).inset).corner_radius(CornerRadius::same(CONTROL_RADIUS)).inner_margin(Margin::same(8))
}

/// Card heading (Fluent "Subtitle"-ish, 18 px semibold) with an optional colored icon.
pub fn card_title(ui: &mut Ui, ic: Option<(Icon, Color32)>, text: &str) {
  match ic {
    Some((i, c)) => {
      let c = adapt(ui, c);
      icon_label(ui, i, 18.0, c, strong(text, 18.0));
    }
    None => {
      ui.label(strong(text, 18.0));
    }
  }
  ui.add_space(4.0);
}

/// Fluent InfoBar: tinted, rounded strip with a severity icon; content to the right.
pub fn info_bar<R>(ui: &mut Ui, severity: Severity, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
  let dark = ui.visuals().dark_mode;
  let (fill, ic) = match (severity, dark) {
    (Severity::Error, true) => (Color32::from_rgb(68, 39, 38), icons::ERROR),
    (Severity::Error, false) => (Color32::from_rgb(253, 231, 233), icons::ERROR),
    (Severity::Warning, true) => (Color32::from_rgb(67, 53, 25), icons::WARNING),
    (Severity::Warning, false) => (Color32::from_rgb(255, 244, 206), icons::WARNING),
    (Severity::Success, true) => (Color32::from_rgb(57, 61, 27), icons::CHECK),
    (Severity::Success, false) => (Color32::from_rgb(223, 246, 221), icons::CHECK),
    (Severity::Info, true) => (Color32::from_rgb(39, 39, 39), icons::INFO),
    (Severity::Info, false) => (Color32::from_rgb(246, 246, 246), icons::INFO),
  };
  let color = severity_color(ui, severity);
  let p = palette(ui);
  Frame::NONE
    .fill(fill)
    .stroke(Stroke::new(1.0_f32, p.card_stroke))
    .corner_radius(CornerRadius::same(CONTROL_RADIUS))
    .inner_margin(Margin::symmetric(14, 10))
    .show(ui, |ui| {
      ui.set_width(ui.available_width());
      ui.horizontal_top(|ui| {
        leading_icon(ui, ic, 16.0, color);
        ui.add_space(4.0);
        ui.vertical(|ui| add_contents(ui)).inner
      })
      .inner
    })
    .inner
}

/// One-line status: colored severity icon + text (secondary color for Info).
pub fn status_line(ui: &mut Ui, severity: Severity, text: &str) -> Response {
  let ic = match severity {
    Severity::Info => icons::INFO,
    Severity::Success => icons::CHECK,
    Severity::Warning => icons::WARNING,
    Severity::Error => icons::ERROR,
  };
  let color = severity_color(ui, severity);
  ui.horizontal_top(|ui| {
    leading_icon(ui, ic, 14.0, color);
    let text_color = if severity == Severity::Info { palette(ui).text_secondary } else { color };
    ui.add(egui::Label::new(RichText::new(text).color(text_color)).wrap())
  })
  .inner
}

/// Filled accent button (Fluent "AccentButton"), for the primary action of a group.
pub fn accent_button(ui: &Ui, text: &str) -> egui::Button<'static> {
  let p = palette(ui);
  egui::Button::new(RichText::new(text.to_string()).color(p.on_accent)).fill(p.accent).stroke(Stroke::NONE)
}

// --- meters --------------------------------------------------------------------------------

/// Level / quantity meter: a thin rounded track filled to `fraction`, the value as text to
/// its right. For levels, percentages and loads; `egui::ProgressBar` is a task progress
/// indicator (text inside the fill, a blob at 0 %) and reads wrong for live values.
pub fn meter(ui: &mut Ui, fraction: f32, width: f32, fill: Color32, value: &str) -> Response {
  let text = RichText::new(value).monospace().color(palette(ui).text_secondary);
  meter_with(ui, fraction, width, fill, text)
}

/// `meter` with the value text styled by the caller (e.g. small, to match captions).
pub fn meter_with(ui: &mut Ui, fraction: f32, width: f32, fill: Color32, value: RichText) -> Response {
  ui.horizontal(|ui| {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    if ui.is_rect_visible(rect) {
      let p = palette(ui);
      let track = egui::Rect::from_center_size(rect.center(), Vec2::new(rect.width(), 6.0));
      let painter = ui.painter();
      painter.rect_filled(track, 3.0, p.inset);
      painter.rect_stroke(track, 3.0, Stroke::new(1.0_f32, p.grid), egui::StrokeKind::Middle);
      let f = fraction.clamp(0.0, 1.0);
      if f > 0.0 {
        let mut filled = track;
        filled.set_width((track.width() * f).max(track.height()));
        painter.rect_filled(filled, 3.0, fill);
      }
    }
    ui.add_space(4.0);
    ui.label(value);
    response
  })
  .inner
}

/// Meter color for a signal level or load (0..1 of full scale): green, amber near the top,
/// red at the top.
/// A volume with a defined 100 %: "80 %", above that the boost: "250 % (+8.0 dB)". Short
/// enough for a mixer strip.
pub fn volume_text(volume: f32) -> String {
  if volume <= 1.0 {
    format!("{:.0} %", volume * 100.0)
  } else {
    format!("{:.0} % (+{:.1} dB)", volume * 100.0, 20.0 * volume.log10())
  }
}

/// Horizontal volume slider 0 ..= `max` with its readout and "Reset to 100 %" (only when `max` is
/// above 100 %). True if changed.
pub fn percent_volume(ui: &mut Ui, volume: &mut f32, max: f32, width: f32, color: Color32) -> bool {
  ui.horizontal(|ui| {
    ui.spacing_mut().slider_width = width;
    let mut changed = ui.add(egui::Slider::new(volume, 0.0..=max).show_value(false)).changed();
    ui.label(strong(volume_text(*volume), 14.0).color(adapt(ui, color)));
    // With 100 % as the top (mic with the leveler on) the slider's end says it already.
    if max > 1.0 && ui.button(crate::i18n::tr("Reset to 100 %")).clicked() {
      *volume = 1.0;
      changed = true;
    }
    changed
  })
  .inner
}

/// A meter's peak (0..1) as text: "42 % (-7.5 dBFS)".
pub fn level_text(peak: f32) -> String {
  if peak > 0.0001 {
    format!("{:.0} % ({:.1} dBFS)", (peak * 100.0).round(), 20.0 * peak.log10())
  } else {
    "0 %".to_string() // "-inf dBFS" means nothing to most people
  }
}

pub fn level_color(ui: &Ui, fraction: f32) -> Color32 {
  let p = palette(ui);
  if fraction > 0.95 {
    p.err
  } else if fraction > 0.8 {
    p.warn
  } else {
    p.ok
  }
}

// --- toggle switch -------------------------------------------------------------------------

/// Fluent toggle switch (40 x 20). Flips `on` when clicked.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
  let size = Vec2::new(40.0, 20.0);
  let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
  if response.clicked() {
    *on = !*on;
    response.mark_changed();
  }
  response.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, ui.is_enabled(), *on, ""));
  if ui.is_rect_visible(rect) {
    let p = palette(ui);
    let t = ui.ctx().animate_bool_responsive(response.id, *on);
    let hovered = response.hovered();
    let radius = rect.height() / 2.0;
    let (fill, stroke, knob) = if *on {
      let fill = if hovered { p.accent.gamma_multiply(0.9) } else { p.accent };
      (fill, Stroke::NONE, p.on_accent)
    } else {
      let fill = if hovered { p.nav_hover } else { Color32::TRANSPARENT };
      (fill, Stroke::new(1.0_f32, p.text_secondary), p.text_secondary)
    };
    let painter = ui.painter();
    painter.rect(rect, radius, fill, stroke, egui::StrokeKind::Middle);
    let knob_r = if hovered { 7.0 } else { 6.0 };
    let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
    painter.circle_filled(egui::pos2(x, rect.center().y), knob_r, knob);
  }
  response
}

/// "Label  [toggle]  On/Off" row piece; returns true when clicked (caller flips the setting).
pub fn toggle_labeled(ui: &mut Ui, on: bool, label: &str) -> bool {
  let mut v = on;
  let mut clicked = false;
  ui.horizontal(|ui| {
    if !label.is_empty() {
      ui.label(label);
    }
    clicked = toggle(ui, &mut v).clicked();
    let state = if on { crate::i18n::tr("On") } else { crate::i18n::tr("Off") };
    ui.label(RichText::new(state).color(palette(ui).text_secondary));
  });
  clicked
}

// --- navigation ----------------------------------------------------------------------------

/// Windows Settings style navigation item: rounded hover/selected fill and an accent pill.
pub fn nav_item(ui: &mut Ui, selected: bool, ic: Icon, label: &str) -> Response {
  let height = 38.0;
  let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
  if ui.is_rect_visible(rect) {
    let p = palette(ui);
    let painter = ui.painter();
    let fill = if selected {
      p.nav_selected
    } else if response.hovered() {
      p.nav_hover
    } else {
      Color32::TRANSPARENT
    };
    painter.rect_filled(rect, CONTROL_RADIUS, fill);
    if selected {
      let pill = egui::Rect::from_center_size(egui::pos2(rect.left() + 1.5, rect.center().y), Vec2::new(3.0, 16.0));
      painter.rect_filled(pill, 1.5, p.accent);
    }
    let label_font = FontId::proportional(14.0);
    let icon_pos =
      egui::pos2(rect.left() + 16.0, rect.center().y + icon_drop(ui.ctx().fonts_mut(|f| f.row_height(&label_font))));
    if ICON_FONT.load(Ordering::Relaxed) {
      painter.text(
        icon_pos,
        Align2::LEFT_CENTER,
        ic.glyph,
        FontId::new(16.0, FontFamily::Name("icons".into())),
        p.text,
      );
    } else {
      painter.text(icon_pos, Align2::LEFT_CENTER, ic.emoji, FontId::proportional(15.0), p.text);
    }
    painter.text(egui::pos2(rect.left() + 46.0, rect.center().y), Align2::LEFT_CENTER, label, label_font, p.text);
  }
  response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Small caps-like section label in the navigation pane.
pub fn nav_section(ui: &mut Ui, text: &str) {
  ui.add_space(10.0);
  ui.horizontal(|ui| {
    ui.add_space(12.0);
    ui.label(RichText::new(text).small().color(palette(ui).text_tertiary));
  });
  ui.add_space(2.0);
}

// --- private -------------------------------------------------------------------------------

/// Paints icon and text as one widget. Separate labels drift apart: a horizontal layout
/// centers each widget in the row height known when it is added (the icon before a taller
/// title gets a shorter row), and the icon font has no descent, so its glyph sits higher
/// in its line box than text does in its own.
fn icon_text(ui: &mut Ui, ic: Icon, size: f32, color: Color32, text: WidgetText, trailing: bool) -> Response {
  let glyph = WidgetText::from(icon(ic, size).color(color)).into_galley(
    ui,
    Some(TextWrapMode::Extend),
    f32::INFINITY,
    TextStyle::Body,
  );
  let label = text.into_galley(ui, Some(TextWrapMode::Extend), f32::INFINITY, TextStyle::Body);
  let gap = ui.spacing().item_spacing.x;
  let (gs, ls) = (glyph.size(), label.size());
  let (rect, response) = ui.allocate_exact_size(Vec2::new(gs.x + gap + ls.x, gs.y.max(ls.y)), Sense::hover());
  if ui.is_rect_visible(rect) {
    let (glyph_x, label_x) =
      if trailing { (rect.left() + ls.x + gap, rect.left()) } else { (rect.left(), rect.left() + gs.x + gap) };
    let drop = icon_drop(ls.y);
    let text_color = ui.visuals().text_color();
    let painter = ui.painter();
    painter.galley(egui::pos2(glyph_x, rect.center().y - gs.y / 2.0 + drop), glyph, color);
    painter.galley(egui::pos2(label_x, rect.center().y - ls.y / 2.0), label, text_color);
  }
  response
}

/// Icon before multi-line content (top aligned), centered on the content's first Body line.
fn leading_icon(ui: &mut Ui, ic: Icon, size: f32, color: Color32) {
  let line = ui.text_style_height(&TextStyle::Body);
  let glyph = ui.ctx().fonts_mut(|f| f.row_height(&FontId::new(size, FontFamily::Name("icons".into()))));
  let offset = if ICON_FONT.load(Ordering::Relaxed) { (line - glyph) / 2.0 + icon_drop(line) } else { 0.0 };
  ui.vertical(|ui| {
    ui.add_space(offset.max(0.0));
    ui.label(icon(ic, size).color(color));
  });
}

/// How far a Fluent glyph moves down to look centered on text of row height `text_row`
/// (between cap-height and x-height middles of Segoe UI). Emoji stand-ins are text, so 0.
fn icon_drop(text_row: f32) -> f32 {
  if ICON_FONT.load(Ordering::Relaxed) { ICON_DROP * text_row } else { 0.0 }
}

fn fonts() -> FontDefinitions {
  let mut fonts = FontDefinitions::default();
  let mut load = |name: &str, files: &[&str]| -> bool {
    for file in files {
      let path = std::path::Path::new(FONT_DIR).join(file);
      match std::fs::read(&path) {
        Ok(bytes) => {
          fonts.font_data.insert(name.to_string(), std::sync::Arc::new(FontData::from_owned(bytes)));
          return true;
        }
        Err(e) => log::debug!("[Theme] font {} not available: {e}", path.display()),
      }
    }
    log::warn!("[Theme] none of {files:?} found; using the bundled font for {name}");
    false
  };
  let text = load("segoe", &["SegUIVar.ttf", "segoeui.ttf"]);
  let semibold = load("segoe-semibold", &["seguisb.ttf"]);
  let icons = load("fluent-icons", &["SegoeIcons.ttf", "segmdl2.ttf"]);
  let mono = load("cascadia", &["CascadiaMono.ttf", "consola.ttf"]);

  let proportional = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
  if text && let Some(list) = fonts.families.get_mut(&FontFamily::Proportional) {
    list.insert(0, "segoe".into());
  }
  if mono && let Some(list) = fonts.families.get_mut(&FontFamily::Monospace) {
    list.insert(0, "cascadia".into());
  }
  if semibold {
    let mut list = vec!["segoe-semibold".to_string()];
    list.extend(proportional.iter().cloned());
    fonts.families.insert(FontFamily::Name("semibold".into()), list);
  }
  if icons {
    fonts.families.insert(FontFamily::Name("icons".into()), vec!["fluent-icons".to_string()]);
  }
  ICON_FONT.store(icons, Ordering::Relaxed);
  SEMIBOLD_FONT.store(semibold, Ordering::Relaxed);
  fonts
}

fn style(p: &Palette) -> egui::Style {
  let mut style = if p.dark { Theme::Dark.default_style() } else { Theme::Light.default_style() };
  let heading_family =
    if SEMIBOLD_FONT.load(Ordering::Relaxed) { FontFamily::Name("semibold".into()) } else { FontFamily::Proportional };
  style.text_styles = [
    (TextStyle::Small, FontId::proportional(12.0)),
    (TextStyle::Body, FontId::proportional(14.0)),
    (TextStyle::Button, FontId::proportional(14.0)),
    (TextStyle::Heading, FontId::new(18.0, heading_family)),
    (TextStyle::Monospace, FontId::monospace(13.0)),
  ]
  .into();

  let s = &mut style.spacing;
  s.item_spacing = Vec2::new(8.0, 8.0);
  s.button_padding = Vec2::new(12.0, 5.0);
  s.interact_size = Vec2::new(40.0, 30.0);
  s.menu_margin = Margin::same(6);
  s.window_margin = Margin::same(20);
  s.indent = 22.0;
  s.slider_width = 200.0;
  s.slider_rail_height = 4.0;
  s.combo_width = 200.0;
  s.text_edit_width = 260.0;
  s.icon_width = 18.0;
  s.icon_width_inner = 10.0;
  s.icon_spacing = 8.0;
  s.tooltip_width = 420.0;
  s.scroll = egui::style::ScrollStyle::floating();

  style.visuals = visuals(p);
  style.interaction.tooltip_delay = 0.4;
  style
}

fn visuals(p: &Palette) -> Visuals {
  let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
  let (bg, control, control_hover, control_press, control_stroke, popup) = if p.dark {
    (
      Color32::from_rgb(32, 32, 32),
      Color32::from_rgb(55, 55, 55),
      Color32::from_rgb(62, 62, 62),
      Color32::from_rgb(47, 47, 47),
      Color32::from_rgb(70, 70, 70),
      Color32::from_rgb(44, 44, 44),
    )
  } else {
    (
      Color32::from_rgb(243, 243, 243),
      Color32::from_rgb(254, 254, 254),
      Color32::from_rgb(249, 249, 249),
      Color32::from_rgb(244, 244, 244),
      Color32::from_rgb(214, 214, 214),
      Color32::from_rgb(252, 252, 252),
    )
  };
  let control_rounding = CornerRadius::same(CONTROL_RADIUS);

  v.panel_fill = bg;
  v.window_fill = popup;
  v.window_stroke = Stroke::new(1.0_f32, p.card_stroke);
  v.window_corner_radius = CornerRadius::same(CARD_RADIUS);
  v.menu_corner_radius = CornerRadius::same(CARD_RADIUS);
  let shadow_alpha = if p.dark { 110 } else { 40 };
  v.window_shadow = Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(shadow_alpha) };
  v.popup_shadow = Shadow { offset: [0, 4], blur: 14, spread: 0, color: Color32::from_black_alpha(shadow_alpha) };
  v.extreme_bg_color = if p.dark { Color32::from_rgb(29, 29, 29) } else { Color32::WHITE };
  v.faint_bg_color = if p.dark { Color32::from_rgb(38, 38, 38) } else { Color32::from_rgb(246, 246, 246) };
  v.code_bg_color = v.faint_bg_color;
  v.hyperlink_color = p.accent;
  v.warn_fg_color = p.warn;
  v.error_fg_color = p.err;
  v.selection.bg_fill = p.accent;
  v.selection.stroke = Stroke::new(1.0_f32, p.on_accent);
  v.slider_trailing_fill = true;
  v.striped = false;
  v.indent_has_left_vline = false;
  v.collapsing_header_frame = false;

  let w = &mut v.widgets;
  w.noninteractive.bg_fill = p.card;
  w.noninteractive.weak_bg_fill = p.card;
  w.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.grid);
  w.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text);
  w.noninteractive.corner_radius = CornerRadius::same(CARD_RADIUS);

  w.inactive.bg_fill = if p.dark { Color32::from_rgb(80, 80, 80) } else { Color32::from_rgb(200, 200, 200) };
  w.inactive.weak_bg_fill = control;
  w.inactive.bg_stroke = Stroke::new(1.0_f32, control_stroke);
  w.inactive.fg_stroke = Stroke::new(1.0_f32, p.text);
  w.inactive.corner_radius = control_rounding;
  w.inactive.expansion = 0.0;

  w.hovered.bg_fill = if p.dark { Color32::from_rgb(95, 95, 95) } else { Color32::from_rgb(185, 185, 185) };
  w.hovered.weak_bg_fill = control_hover;
  w.hovered.bg_stroke = Stroke::new(1.0_f32, control_stroke);
  w.hovered.fg_stroke = Stroke::new(1.0_f32, p.text);
  w.hovered.corner_radius = control_rounding;
  w.hovered.expansion = 0.0;

  w.active.bg_fill = p.accent;
  w.active.weak_bg_fill = control_press;
  w.active.bg_stroke = Stroke::new(1.0_f32, p.accent);
  w.active.fg_stroke = Stroke::new(1.0_f32, p.text_secondary);
  w.active.corner_radius = control_rounding;
  w.active.expansion = 0.0;

  w.open.bg_fill = control_press;
  w.open.weak_bg_fill = control_press;
  w.open.bg_stroke = Stroke::new(1.0_f32, control_stroke);
  w.open.fg_stroke = Stroke::new(1.0_f32, p.text);
  w.open.corner_radius = control_rounding;
  w.open.expansion = 0.0;
  v
}

/// `egui::Widget` adapter so a toggle can be `ui.add`ed.
pub struct Toggle<'a>(pub &'a mut bool);

impl Widget for Toggle<'_> {
  fn ui(self, ui: &mut Ui) -> Response {
    toggle(ui, self.0)
  }
}
