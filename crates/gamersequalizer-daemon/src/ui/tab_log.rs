//! Log page: tail of the shared log file (UI + background service), filterable. Behaves like
//! a terminal: it follows the newest line until you scroll up, and follows again once you
//! scroll back to the bottom. Reads the file at most once a second, only while shown.

use super::state::AppState;
use super::theme::{self, icons};
use crate::app_log::{self, LineLevel, LogLine};
use crate::i18n::{tk, tr, trf};
use egui::{Color32, RichText, Ui};
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const REFRESH: Duration = Duration::from_secs(1);
/// ~2000 typical lines; older entries stay in the file.
const TAIL_BYTES: u64 = 256 * 1024;
/// Within this distance of the end counts as "at the bottom" (keeps following).
const BOTTOM_SLACK: f32 = 4.0;

/// Topics the log can be narrowed to (matched against the "[Tag]" prefixes in messages).
const TOPICS: [(&str, &[&str]); 3] = [
  (tk("All"), &[]),
  (tk("Game detection"), &["[Game]", "[OutputEq]"]),
  (tk("Microphone"), &["[Mic", "[Denoise]", "[AEC]", "[ApoLink]", "[DSP]"]),
];
/// Most severe first; a choice shows itself and everything above it.
const LEVELS: [(LineLevel, &str); 4] = [
  (LineLevel::Error, tk("Errors")),
  (LineLevel::Warn, tk("Warnings")),
  (LineLevel::Info, tk("Info")),
  (LineLevel::Debug, tk("Debug")),
];

pub struct LogView {
  lines: Vec<LogLine>,
  read_at: Option<Instant>,
  error: Option<String>,
  topic: usize,
  level: LineLevel,
  libraries: bool,
  search: String,
  /// Scroll to the newest line on the next frame (first show, filter change, "Newest").
  jump_to_end: bool,
  /// The view showed the last line last frame (egui then keeps following it).
  at_end: bool,
}

impl Default for LogView {
  fn default() -> Self {
    Self {
      lines: Vec::new(),
      read_at: None,
      error: None,
      topic: 0,
      level: LineLevel::Info,
      libraries: false,
      search: String::new(),
      jump_to_end: true,
      at_end: true,
    }
  }
}

pub fn render_tab_log(ui: &mut Ui, state: &mut AppState) {
  let view = &mut state.log_view;
  if view.read_at.is_none_or(|t| t.elapsed() >= REFRESH) {
    view.read_at = Some(Instant::now());
    match app_log::read_tail(&app_log::log_path(), TAIL_BYTES) {
      Ok(lines) => {
        view.lines = lines;
        view.error = None;
      }
      Err(e) => view.error = Some(e.to_string()),
    }
  }
  ui.ctx().request_repaint_after(REFRESH);

  theme::page_header(
    ui,
    icons::LOG,
    Color32::from_rgb(150, 150, 150),
    tr("Log"),
    Some(tr("What the app and the background service detected and did: game detection, profile changes, microphone processing, CPU overload.")),
  );
  let mut filters_changed = false;
  ui.horizontal_wrapped(|ui| {
    for (i, (name, _)) in TOPICS.iter().enumerate() {
      filters_changed |= ui.selectable_value(&mut view.topic, i, tr(name)).changed();
    }
    ui.separator();
    ui.label(tr("Level:"));
    for (level, name) in LEVELS {
      filters_changed |= ui.selectable_value(&mut view.level, level, tr(name)).changed();
    }
    ui.separator();
    filters_changed |= ui
      .checkbox(&mut view.libraries, tr("Library messages"))
      .on_hover_text(tr("Also show messages from libraries (graphics driver, window toolkit)."))
      .changed();
  });
  ui.horizontal(|ui| {
    ui.label(tr("Search:"));
    filters_changed |= ui.text_edit_singleline(&mut view.search).changed();
    if ui.button(format!("📂  {}", tr("Open log folder"))).clicked() {
      open_folder();
    }
    if !view.at_end
      && ui
        .button(format!("⬇  {}", tr("Newest")))
        .on_hover_text(tr("Jump to the end and follow new lines again."))
        .clicked()
    {
      view.jump_to_end = true;
    }
  });
  if let Some(e) = view.error.as_deref() {
    ui.label(RichText::new(trf("Log file not readable yet: {}", &[&e])).color(theme::palette(ui).warn));
  }
  // A different filter means different content: show its newest lines.
  if filters_changed {
    view.jump_to_end = true;
  }

  let search = view.search.to_lowercase();
  let (_, tags) = TOPICS.get(view.topic).copied().unwrap_or(("All", &[]));
  let shown: Vec<&LogLine> = view
    .lines
    .iter()
    .filter(|l| l.level <= view.level)
    .filter(|l| view.libraries || l.own)
    .filter(|l| tags.is_empty() || tags.iter().any(|t| l.text.contains(t)))
    .filter(|l| search.is_empty() || l.text.to_lowercase().contains(&search))
    .collect();

  ui.separator();
  let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
  // egui keeps following the end by itself once the offset sits there (stick_to_bottom), and
  // stops/resumes when the user scrolls away/back. We only jump to the end once on request.
  let mut area = egui::ScrollArea::both().id_salt("log_lines").auto_shrink([false, false]).stick_to_bottom(true);
  if view.jump_to_end {
    view.jump_to_end = false;
    // Past the end on purpose; egui clamps it to the end after this frame and sticks there.
    // Must be finite: `show_rows` derives the row range from it and f32::MAX overflows to an
    // empty range. The overshot frame draws nothing, so paint the next one right away.
    let content_height = shown.len() as f32 * (row_height + ui.spacing().item_spacing.y);
    area = area.vertical_scroll_offset(content_height);
    ui.ctx().request_repaint();
  }
  let out = area.show_rows(ui, row_height, shown.len(), |ui, range| {
    for line in shown.get(range).unwrap_or(&[]) {
      let text = RichText::new(&line.text).monospace();
      let text = match line.level {
        LineLevel::Error => text.color(theme::palette(ui).err),
        LineLevel::Warn => text.color(theme::palette(ui).warn),
        LineLevel::Info => text,
        LineLevel::Debug => text.weak(),
      };
      ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Extend));
    }
  });
  let max_offset = (out.content_size.y - out.inner_rect.height()).max(0.0);
  view.at_end = out.state.offset.y >= max_offset - BOTTOM_SLACK;
}

fn open_folder() {
  let dir = app_log::log_dir();
  let wide: Vec<u16> = dir.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
  // SAFETY: both strings are NUL-terminated and outlive the call.
  unsafe {
    ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), None, None, SW_SHOWNORMAL);
  }
}
