//! Log page: tail of the shared log file (UI + background service), filterable. Behaves like
//! a terminal: it follows the newest line until you scroll up, and follows again once you
//! scroll back to the bottom. Reads the file at most once a second, only while shown.

use super::state::AppState;
use super::theme::{self, icons};
use crate::app_log::{self, LineLevel, LogLine};
use crate::i18n::{tk, tr, trf};
use crate::output_eq::autoeq::downloads_dir;
use egui::{Color32, RichText, Ui};
use std::path::PathBuf;
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
  (tk("Microphone"), &["[Mic", "[Denoise]", "[AEC]", "[ApoLink]", "[DSP]", "[Leveler]"]),
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
  /// Indices into `lines` that pass the filters; recomputed only after a reload or a filter
  /// change (filtering ~2000 lines with a lowercase copy each, every frame, was ~66k
  /// allocations a second).
  shown: Vec<usize>,
  shown_stale: bool,
  /// True while text is selected or being dragged. Holds reload still so the selection doesn't shift.
  selecting: bool,
  /// Currently selected text in the log, if any.
  selected_text: Option<String>,
  /// Combined text of all lines matching the filters.
  buffer: String,
  /// Cached layout job with severity colors.
  layout_job: egui::text::LayoutJob,
  /// Remembers theme so switching dark/light updates line colors in the cached layout.
  dark_mode: Option<bool>,
  /// Result of the last "Save a copy to send": where it went, or why it failed.
  saved: Option<String>,
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
      shown: Vec::new(),
      shown_stale: true,
      selecting: false,
      selected_text: None,
      buffer: String::new(),
      layout_job: egui::text::LayoutJob::default(),
      dark_mode: None,
      saved: None,
    }
  }
}

pub fn render_tab_log(ui: &mut Ui, state: &mut AppState) {
  let view = &mut state.log_view;
  // Hold reload still while text is selected or being dragged so rows don't shift under the cursor.
  // Clicking anywhere to clear the selection resumes reloads on the next interval.
  let selecting = view.selecting;
  if !selecting && view.read_at.is_none_or(|t| t.elapsed() >= REFRESH) {
    view.read_at = Some(Instant::now());
    match app_log::read_tail(&app_log::log_path(), TAIL_BYTES) {
      Ok(lines) => {
        view.lines = lines;
        view.error = None;
        view.shown_stale = true;
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
    ui.separator();
    let mut save_debug = app_log::debug_to_disk();
    if ui
      .checkbox(&mut save_debug, tr("Save debug to disk"))
      .on_hover_text(tr("Also write detailed debug lines to the log file, e.g. every voice leveler change and why. Off: they are not recorded at all."))
      .changed()
    {
      app_log::set_debug_to_disk(save_debug);
    }
  });
  ui.horizontal(|ui| {
    ui.label(tr("Search:"));
    filters_changed |= ui.text_edit_singleline(&mut view.search).changed();
    if ui.button(format!("📂  {}", tr("Open log folder"))).clicked() {
      open_folder();
    }
    let save = ui.button(tr("Save a copy to send")).on_hover_text(tr(
      "Copies the log into your Downloads folder and shows it there. The log itself sits in a hidden folder and \
       keeps changing while the app runs, so a browser or a messenger may refuse to upload it.",
    ));
    if save.clicked() {
      view.saved = Some(match save_copy() {
        Ok(path) => trf("Saved: {}", &[&path.display()]),
        Err(e) => trf("The copy could not be saved: {}", &[&e]),
      });
    }
    let copy_label = if view.selected_text.is_some() {
      tr("Copy selection")
    } else {
      tr("Copy all")
    };
    if ui
      .button(format!("📋  {}", copy_label))
      .on_hover_text(tr("Copies to the Windows clipboard (Ctrl+C)."))
      .clicked()
    {
      let text = view.selected_text.as_deref().unwrap_or(&view.buffer);
      view.saved = Some(match crate::win_clipboard::set_clipboard_text(text) {
        Ok(()) => tr("Copied to clipboard.").to_string(),
        Err(e) => trf("Copy failed: {}", &[&e]),
      });
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
  if let Some(saved) = view.saved.as_deref() {
    ui.label(RichText::new(saved).small());
  }
  if let Some(e) = view.error.as_deref() {
    ui.label(RichText::new(trf("Log file not readable yet: {}", &[&e])).color(theme::palette(ui).warn));
  }
  // A different filter means different content: show its newest lines.
  if filters_changed {
    view.jump_to_end = true;
  }

  let dark_mode = ui.visuals().dark_mode;
  let theme_changed = view.dark_mode != Some(dark_mode);
  if theme_changed {
    view.dark_mode = Some(dark_mode);
  }

  if filters_changed || view.shown_stale || theme_changed {
    view.shown_stale = false;
    let search = view.search.to_lowercase();
    let (_, tags) = TOPICS.get(view.topic).copied().unwrap_or(("All", &[]));
    let (level, libraries) = (view.level, view.libraries);
    view.shown = view
      .lines
      .iter()
      .enumerate()
      .filter(|(_, l)| l.level <= level)
      .filter(|(_, l)| libraries || l.own)
      .filter(|(_, l)| tags.is_empty() || tags.iter().any(|t| l.text.contains(t)))
      .filter(|(_, l)| search.is_empty() || l.text.to_lowercase().contains(&search))
      .map(|(i, _)| i)
      .collect();

    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    let palette = theme::palette(ui);
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    for &i in &view.shown {
      if let Some(line) = view.lines.get(i) {
        let color = match line.level {
          LineLevel::Error => palette.err,
          LineLevel::Warn => palette.warn,
          LineLevel::Info => ui.style().visuals.text_color(),
          LineLevel::Debug => ui.style().visuals.weak_text_color(),
        };
        let format = egui::text::TextFormat::simple(font_id.clone(), color);
        job.append(&line.text, 0.0, format.clone());
        job.append("\n", 0.0, format);
      }
    }
    view.buffer = job.text.clone();
    view.layout_job = job;
  }

  ui.separator();
  let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
  // egui keeps following the end by itself once the offset sits there (stick_to_bottom), and
  // stops/resumes when the user scrolls away/back. We only jump to the end once on request.
  let mut area = egui::ScrollArea::both()
    .id_salt("log_lines")
    .auto_shrink([false, false])
    .stick_to_bottom(!view.selecting);
  if view.jump_to_end {
    view.jump_to_end = false;
    // Past the end on purpose; egui clamps it to the end after this frame and sticks there.
    let content_height = view.shown.len() as f32 * row_height * 2.0;
    area = area.vertical_scroll_offset(content_height);
    ui.ctx().request_repaint();
  }
  let cached_job = view.layout_job.clone();
  let mut layouter = move |ui: &Ui, _buf: &dyn egui::text_edit::TextBuffer, _wrap_width: f32| {
    ui.fonts_mut(|f| f.layout_job(cached_job.clone()))
  };
  let out = area.show(ui, |ui| {
    let mut text_ref = view.buffer.as_str();
    egui::TextEdit::multiline(&mut text_ref)
      .id_salt("log_text")
      .font(egui::TextStyle::Monospace)
      .frame(egui::Frame::NONE)
      .desired_width(f32::INFINITY)
      .layouter(&mut layouter)
      .show(ui)
  });
  let max_offset = (out.content_size.y - out.inner_rect.height()).max(0.0);
  view.at_end = out.state.unclamped_offset().y >= max_offset - BOTTOM_SLACK;
  let (has_selection, selected_text) = match out.inner.cursor_range {
    Some(range) if range.primary != range.secondary => (true, Some(range.slice_str(&view.buffer).to_string())),
    _ => (false, None),
  };
  let is_pointer_down = out.inner.response.is_pointer_button_down_on();
  view.selecting = has_selection || is_pointer_down;
  view.selected_text = selected_text.clone();

  out.inner.response.context_menu(|ui| {
    if let Some(ref text) = selected_text {
      if ui.button(tr("Copy selection")).clicked() {
        let _ = crate::win_clipboard::set_clipboard_text(text);
        ui.close();
      }
    }
    if ui.button(tr("Copy all")).clicked() {
      let _ = crate::win_clipboard::set_clipboard_text(&view.buffer);
      ui.close();
    }
  });

  let ctrl_c = ui.input(|i| i.modifiers.command_only() && i.key_pressed(egui::Key::C))
    || ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
  if ctrl_c {
    if let Some(ref text) = selected_text {
      let _ = crate::win_clipboard::set_clipboard_text(text);
    }
  }
}

/// Copies the log to Downloads under a new, time-stamped name and shows it in Explorer: a
/// file that does not change and sits where upload dialogs start.
fn save_copy() -> Result<PathBuf, String> {
  let dir = downloads_dir().ok_or("no Downloads folder")?;
  let name = format!("GamersEqualizer-log-{}.txt", jiff::Zoned::now().strftime("%Y%m%d-%H%M%S"));
  let target = dir.join(name);
  std::fs::copy(app_log::log_path(), &target).map_err(|e| e.to_string())?;
  let select: Vec<u16> = format!("/select,\"{}\"", target.display()).encode_utf16().chain(std::iter::once(0)).collect();
  // SAFETY: both strings are NUL-terminated and outlive the call.
  unsafe {
    ShellExecuteW(None, w!("open"), w!("explorer.exe"), PCWSTR(select.as_ptr()), None, SW_SHOWNORMAL);
  }
  Ok(target)
}

fn open_folder() {
  let dir = app_log::log_dir();
  let wide: Vec<u16> = dir.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
  // SAFETY: both strings are NUL-terminated and outlive the call.
  unsafe {
    ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), None, None, SW_SHOWNORMAL);
  }
}
