//! Headphones tab: one-click switching between the user's headphones (the choice is
//! remembered per output device), an ON/OFF switch to compare, the status of the output APO
//! that applies the correction, and a step-by-step wizard that gets an AutoEQ profile into
//! the app without the app going online: it says where to click, finds the downloaded file
//! and checks it before saving.

use super::eq_editor::{band_editor, on_off_button, response_plot};
use super::state::{AppState, OutputLayer};
use super::theme::{self, icons, Severity};
use crate::i18n::{tr, trf};
use crate::output_eq::autoeq::{self, AutoEqEntry, Check, CheckLevel, DownloadsScan, ParsedEq};
use crate::win_audio::apo_install::{self, ApoStatus, Flow};
use crate::win_audio::{shell, WinAudioSystem};
use egui::{Color32, RichText, Ui};
use gamersequalizer_common::biquad::MAX_LAYER_EQ_BANDS;
use gamersequalizer_common::profile::{ParametricProfile, VadType};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

const AUTOEQ_REPO_URL: &str = "https://github.com/jaakkopasanen/AutoEq";
const AUTOEQ_WEB_URL: &str = "https://autoeq.app";
const COLOR: Color32 = Color32::from_rgb(100, 150, 220);
const APO_RECHECK: Duration = Duration::from_secs(3);
/// The wizard looks for the downloaded file this often (one directory listing).
const DOWNLOADS_RECHECK: Duration = Duration::from_secs(2);

/// Steps of the "add headphones" wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Step {
  #[default]
  Choose,
  /// Path A: one model from autoeq.app.
  WebDownload,
  WebFind,
  /// Path B: the whole database from GitHub.
  ZipDownload,
  ZipFolder,
  Pick,
  /// Check the file, name it, save.
  Review,
  Done,
}

/// A file (or pasted text) waiting for the user's OK.
#[derive(Debug, Clone)]
struct Candidate {
  name: String,
  /// Where it came from, shown to the user.
  origin: String,
  parsed: Option<ParsedEq>,
  checks: Vec<Check>,
}

/// UI-only state of the tab (not persisted, except the AutoEQ folder in the settings).
#[derive(Default)]
pub struct HeadphonesUi {
  pub filter: String,
  wizard_open: Option<bool>,
  step: Step,
  /// Step to return to from Review.
  review_back: Step,
  candidate: Option<Candidate>,
  downloads: DownloadsScan,
  downloads_checked: Option<Instant>,
  /// First existing AutoEQ folder (`autoeq::candidate_dirs`), re-checked with the downloads:
  /// each check is several `is_dir` calls, and a configured network folder that is offline
  /// froze the UI for seconds when it ran every frame.
  autoeq_found: Option<PathBuf>,
  /// Search results for the query they were computed for (cleared when the index changes).
  search_hits: Option<(String, Vec<AutoEqEntry>)>,
  folder_text: String,
  query: String,
  entries: Vec<AutoEqEntry>,
  scanning: Option<Receiver<(PathBuf, Vec<AutoEqEntry>)>>,
  paste_text: String,
  paste_name: String,
  message: Option<String>,
  saved_name: Option<String>,
  query_seeded_for: Option<String>,
  apo: Option<(apo_install::ApoCheck, Instant)>,
  initialized: bool,
}

impl HeadphonesUi {
  /// Show the "add headphones" wizard from its first step.
  pub fn open_wizard(&mut self) {
    self.wizard_open = Some(true);
    self.step = Step::Choose;
    self.candidate = None;
  }

  /// Re-read output APO status on the next frame (after switching outputs or installing).
  pub fn recheck_apo(&mut self) {
    self.apo = None;
  }
}

pub fn render_tab_headphones(ui: &mut Ui, state: &mut AppState, win_audio: &mut WinAudioSystem) {
  let device_id = win_audio.chatmix.master_device_id.clone();
  let device_name = state
    .endpoints
    .iter()
    .find(|e| Some(&e.id) == device_id.as_ref())
    .map(|e| e.name.clone())
    .unwrap_or_else(|| tr("Default output").to_string());
  init_ui(state, &device_name);

  theme::page_header(
    ui,
    icons::HEADPHONES,
    COLOR,
    tr("Output & headphones"),
    Some(tr("Pick where the sound goes, then the correction for it. Speakers or a headset without a profile: None.")),
  );
  super::header::render_device_card(ui, state, win_audio);
  output_apo_status(ui, state, device_id.as_deref());
  if !state.output_eq.settings.enabled {
    theme::info_bar(ui, Severity::Warning, |ui| {
      ui.label(tr("The master switch in the header is OFF: nothing is applied right now."));
    });
    ui.add_space(theme::CARD_GAP);
  }

  theme::card(ui, |ui| {
    my_headphones_row(ui, state);
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| super::profile_bar::render_output_buttons(ui, state, OutputLayer::Headphone));
    super::profile_bar::render_output_notice(ui, state, OutputLayer::Headphone);
    if state.output_eq.headphone_profile().is_some() {
      ui.add_space(4.0);
      super::tab_eq::volume_row(ui, state, OutputLayer::Headphone);
    }
  });

  // The wizard sits above the editor: below it, it was off-screen on normal window heights.
  let open = *state.headphones_ui.wizard_open.get_or_insert(state.output_eq.headphone_profiles.is_empty());
  if open {
    theme::card(ui, |ui| wizard(ui, state, &device_name));
  }

  theme::card(ui, |ui| {
    let height = super::eq_editor::plot_height(ui, 0.28, 150.0, 380.0);
    let mut changed = false;
    if let Some(profile) = state.output_eq.headphone_profile_mut() {
      changed = band_editor(ui, "hp_eq", &mut profile.bands, MAX_LAYER_EQ_BANDS, COLOR, &[], height);
    } else {
      response_plot(ui, "hp_response", &[], COLOR, height);
      ui.label(theme::caption(tr("Select a headphone profile above, click \"Add from AutoEQ…\", or click \"New…\".")));
    }
    if changed {
      state.apply_output_eq();
      state.out_eq_unsaved = Some(OutputLayer::Headphone);
    }
  });
}

/// One button per imported headphone profile: one click switches (and is remembered for
/// this output device). Different output devices switch automatically.
fn my_headphones_row(ui: &mut Ui, state: &mut AppState) {
  ui.horizontal_wrapped(|ui| {
    let on = state.output_eq.settings.headphone_enabled;
    if on_off_button(ui, on, tr("Headphone EQ")) {
      state.output_eq.settings.headphone_enabled = !on;
      state.output_eq_do(|eq, shm| eq.save_settings(shm));
    }
  });
  ui.add_space(4.0);
  ui.horizontal_wrapped(|ui| {
    ui.label(tr("My headphones")).on_hover_text(tr(
      "Click to switch. The choice is remembered for the current output device, so a USB headset and \
       headphones on the jack switch by themselves when you change the output.",
    ));
    let current = state.output_eq.headphone_profile_name().map(str::to_string);
    let mut picked: Option<Option<String>> = None;
    if ui.selectable_label(current.is_none(), tr("None (speakers, flat)")).clicked() {
      picked = Some(None);
    }
    for profile in &state.output_eq.headphone_profiles {
      if ui.selectable_label(current.as_deref() == Some(profile.name.as_str()), &profile.name).clicked() {
        picked = Some(Some(profile.name.clone()));
      }
    }
    if let Some(choice) = picked {
      state.out_eq_unsaved = None;
      state.output_eq_do(|eq, shm| eq.select_headphone(choice, shm));
    }
    if ui
      .button(theme::with_icon(icons::HEADPHONES, tr("Add from AutoEQ…")))
      .on_hover_text(tr("Step-by-step: get the correction for your model from AutoEQ"))
      .clicked()
    {
      state.headphones_ui.open_wizard();
    }
  });
}

// --- wizard ----------------------------------------------------------------------------------

fn wizard(ui: &mut Ui, state: &mut AppState, device_name: &str) {
  poll_index(ui, state);
  let step = state.headphones_ui.step;
  ui.horizontal(|ui| {
    theme::card_title(ui, Some((icons::HEADPHONES, COLOR)), tr("Add headphones from AutoEQ"));
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      if ui.button(theme::with_icon(icons::CLOSE, tr("Close"))).clicked() {
        state.headphones_ui.wizard_open = Some(false);
      }
    });
  });
  ui.label(theme::caption(tr(
    "AutoEQ is a free database of measured corrections for thousands of headphones. This app never goes \
     online itself: you download the file in your browser, the app finds it and checks it.",
  )));
  ui.add_space(8.0);
  match step {
    Step::Choose => step_choose(ui, state),
    Step::WebDownload => step_web_download(ui, state),
    Step::WebFind => step_web_find(ui, state),
    Step::ZipDownload => step_zip_download(ui, state),
    Step::ZipFolder => step_zip_folder(ui, state),
    Step::Pick => step_pick(ui, state),
    Step::Review => step_review(ui, state, device_name),
    Step::Done => step_done(ui, state, device_name),
  }
}

/// A big choice button with a caption below (Windows "settings card" style).
fn choice(ui: &mut Ui, title: &str, caption: &str) -> bool {
  let clicked =
    ui.add_sized([ui.available_width().min(560.0), 34.0], egui::Button::new(theme::strong(title, 14.0))).clicked();
  ui.label(theme::caption(caption));
  ui.add_space(4.0);
  clicked
}

fn step_choose(ui: &mut Ui, state: &mut AppState) {
  ui.label(theme::strong(tr("How do you want to get it?"), 15.0));
  ui.add_space(4.0);
  if choice(
    ui,
    tr("A)  One pair of headphones  (recommended, about 1 minute)"),
    tr("Download one small text file from autoeq.app."),
  ) {
    go(state, Step::WebDownload);
  }
  if choice(
    ui,
    tr("B)  The whole database  (offline search, one large download)"),
    tr("Download the AutoEq repository as a ZIP once (hundreds of MB), then search all models offline."),
  ) {
    go(state, Step::ZipDownload);
  }
  if ui.button(tr("C)  I already have a ParametricEQ.txt file...")).clicked() {
    if let Some(path) = shell::pick_text_file(tr("AutoEQ / Equalizer APO parametric EQ")) {
      review_file(state, &path, None, Step::Choose);
    }
  }
  ui.add_space(4.0);
  egui::CollapsingHeader::new(tr("D)  Paste the text instead")).show(ui, |ui| paste_box(ui, state));
}

fn step_web_download(ui: &mut Ui, state: &mut AppState) {
  ui.label(theme::strong(tr("Step 1 of 3: download the file in your browser"), 15.0));
  numbered(ui, 1, tr("Click the button below. autoeq.app opens in your normal browser."));
  if ui
    .add(theme::accent_button(ui, &theme::with_icon(icons::GLOBE, tr("Open autoeq.app"))))
    .on_hover_text(AUTOEQ_WEB_URL)
    .clicked()
  {
    shell::open_url(AUTOEQ_WEB_URL);
  }
  numbered(ui, 2, tr("Type your headphone model into the search box and pick it from the list."));
  numbered(ui, 3, tr("As the equalizer app choose \"EqualizerAPO ParametricEq\" (not GraphicEq)."));
  numbered(ui, 4, tr("Click Download. The browser saves \"<model> ParametricEQ.txt\" to Downloads."));
  ui.label(theme::caption(tr("Not the GraphicEQ file: the app will tell you if you picked the wrong one.")));
  nav(ui, state, Step::Choose, Some((tr("Next: find the file"), Step::WebFind)));
}

fn step_web_find(ui: &mut Ui, state: &mut AppState) {
  ui.label(theme::strong(tr("Step 2 of 3: find the downloaded file"), 15.0));
  refresh_downloads(ui, state);
  let dir = autoeq::downloads_dir().map(|d| d.display().to_string()).unwrap_or_else(|| tr("Downloads").into());
  let scan = state.headphones_ui.downloads.clone();
  if scan.parametric_files.is_empty() {
    ui.horizontal(|ui| {
      ui.spinner();
      ui.label(trf("Waiting for a \"... ParametricEQ.txt\" in {} (checked every 2 s)...", &[&dir]));
    });
  } else {
    theme::status_line(ui, Severity::Success, tr("Found in Downloads (newest first):"));
  }
  for path in scan.parametric_files.iter().take(5) {
    ui.horizontal(|ui| {
      if ui.button(tr("Use this")).clicked() {
        review_file(state, path, None, Step::WebFind);
      }
      ui.label(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    });
  }
  if let Some(wrong) = scan.other_variants.first() {
    let name = wrong.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    theme::status_line(
      ui,
      Severity::Warning,
      &trf("\"{}\" is a different variant. On autoeq.app choose a Parametric EQ app and download again.", &[&name]),
    );
  }
  if ui.button(format!("📂  {}", tr("Pick the file myself..."))).clicked() {
    if let Some(path) = shell::pick_text_file(tr("AutoEQ / Equalizer APO parametric EQ")) {
      review_file(state, &path, None, Step::WebFind);
    }
  }
  nav(ui, state, Step::WebDownload, None);
}

fn step_zip_download(ui: &mut Ui, state: &mut AppState) {
  ui.label(theme::strong(tr("Step 1 of 3: download and extract the database"), 15.0));
  numbered(ui, 1, tr("Click the button below. The AutoEq page on GitHub opens in your browser."));
  if ui
    .add(theme::accent_button(ui, &theme::with_icon(icons::GLOBE, tr("Open AutoEq on GitHub"))))
    .on_hover_text(AUTOEQ_REPO_URL)
    .clicked()
  {
    shell::open_url(AUTOEQ_REPO_URL);
  }
  numbered(ui, 2, tr("Click the green \"Code\" button, then \"Download ZIP\". It is large; let it finish."));
  numbered(ui, 3, tr("In Downloads right-click \"AutoEq-master.zip\" > \"Extract All...\" > Extract."));
  refresh_downloads(ui, state);
  let found = state.headphones_ui.autoeq_found.clone();
  match (&found, &state.headphones_ui.downloads.autoeq_zip) {
    (Some(dir), _) => {
      theme::status_line(ui, Severity::Success, &trf("Extracted database found: {}", &[&dir.display()]));
    }
    (None, Some(zip)) => {
      theme::status_line(
        ui,
        Severity::Warning,
        &trf("{} is downloaded but not extracted yet (step 3).", &[&zip.display()]),
      );
    }
    (None, None) => {
      ui.horizontal(|ui| {
        ui.spinner();
        ui.label(tr("Waiting for the extracted folder in Downloads (checked every 2 s)..."));
      });
    }
  }
  if let Some(dir) = found {
    if state.headphones_ui.folder_text.is_empty() {
      state.headphones_ui.folder_text = dir.display().to_string();
    }
  }
  nav(ui, state, Step::Choose, Some((tr("Next: index it"), Step::ZipFolder)));
}

fn step_zip_folder(ui: &mut Ui, state: &mut AppState) {
  ui.label(theme::strong(tr("Step 2 of 3: index the database"), 15.0));
  ui.label(tr("The app reads the folder once (a few seconds) and remembers where it is."));
  ui.horizontal(|ui| {
    ui.label(tr("Folder:"));
    ui.add(
      egui::TextEdit::singleline(&mut state.headphones_ui.folder_text)
        .desired_width(380.0)
        .hint_text(r"C:\Users\...\Downloads\AutoEq-master"),
    );
    let scanning = state.headphones_ui.scanning.is_some();
    let label = if scanning { tr("Indexing...") } else { tr("Index") };
    if ui.add_enabled(!scanning, egui::Button::new(label)).clicked() {
      let dir = PathBuf::from(state.headphones_ui.folder_text.trim());
      let dir = if dir.join("results").is_dir() { dir.join("results") } else { dir };
      start_scan(&mut state.headphones_ui, dir);
    }
  });
  if let Some(msg) = state.headphones_ui.message.as_deref() {
    ui.label(RichText::new(msg).small());
  }
  let next = (!state.headphones_ui.entries.is_empty()).then_some((tr("Next: find your model"), Step::Pick));
  nav(ui, state, Step::ZipDownload, next);
}

fn step_pick(ui: &mut Ui, state: &mut AppState) {
  ui.label(theme::strong(tr("Step 3 of 3: find your model"), 15.0));
  ui.label(theme::caption(tr(
    "Several results per model = different measurements; oratory1990 or crinacle are a good first choice.",
  )));
  ui.horizontal(|ui| {
    ui.label(tr("Search:"));
    ui.add(
      egui::TextEdit::singleline(&mut state.headphones_ui.query).desired_width(280.0).hint_text(tr("e.g. HD 600")),
    );
  });
  let mut chosen: Option<AutoEqEntry> = None;
  theme::inset_frame(ui).show(ui, |ui| {
    ui.set_width(ui.available_width());
    egui::ScrollArea::vertical().id_salt("autoeq_results").max_height(200.0).show(ui, |ui| {
      let ui_state = &mut state.headphones_ui;
      if ui_state.search_hits.as_ref().is_none_or(|(q, _)| *q != ui_state.query) {
        let hits = autoeq::search(&ui_state.entries, &ui_state.query).into_iter().cloned().collect();
        ui_state.search_hits = Some((ui_state.query.clone(), hits));
      }
      let results: Vec<&AutoEqEntry> = ui_state.search_hits.iter().flat_map(|(_, hits)| hits).collect();
      if results.is_empty() {
        ui.label(theme::caption(tr(
          "No model matches; try fewer words (\"HD 600\" instead of \"Sennheiser HD 600 Open\").",
        )));
      }
      for entry in results {
        ui.horizontal(|ui| {
          if ui.button(tr("Use")).clicked() {
            chosen = Some(entry.clone());
          }
          ui.label(RichText::new(&entry.name).strong());
          ui.label(theme::caption(&entry.source));
        });
      }
    });
  });
  if let Some(entry) = chosen {
    review_file(state, &entry.path, Some(&entry.name), Step::Pick);
  }
  nav(ui, state, Step::ZipFolder, None);
}

fn step_review(ui: &mut Ui, state: &mut AppState, device_name: &str) {
  ui.label(theme::strong(tr("Check before saving"), 15.0));
  let Some(mut candidate) = state.headphones_ui.candidate.clone() else {
    go(state, Step::Choose);
    return;
  };
  ui.label(theme::caption(trf("From: {}", &[&candidate.origin])));
  for check in &candidate.checks {
    let severity = match check.level {
      CheckLevel::Ok => Severity::Success,
      CheckLevel::Warning => Severity::Warning,
      CheckLevel::Error => Severity::Error,
    };
    theme::status_line(ui, severity, &check.text);
  }
  let usable = candidate.parsed.is_some() && candidate.checks.iter().all(|c| c.level != CheckLevel::Error);
  if let Some(parsed) = candidate.parsed.as_ref() {
    response_plot(ui, "hp_review_plot", &parsed.bands, COLOR, 120.0);
  }
  ui.horizontal(|ui| {
    ui.label(tr("Name:"));
    ui.add(egui::TextEdit::singleline(&mut candidate.name).desired_width(280.0));
  });
  let exists = state.output_eq.layer_profile_exists(OutputLayer::Headphone, &candidate.name);
  if exists {
    theme::status_line(ui, Severity::Warning, tr("A profile with this name exists and will be replaced."));
  }
  let back = state.headphones_ui.review_back;
  let mut save = false;
  ui.horizontal(|ui| {
    if ui.button(format!("◀  {}", tr("Back"))).clicked() {
      go(state, back);
    }
    let label = trf("💾  Save and use on {}", &[&super::truncate_chars(device_name, 28)]);
    save = ui.add_enabled(usable && !candidate.name.trim().is_empty(), theme::accent_button(ui, &label)).clicked();
  });
  if save {
    if let Some(parsed) = candidate.parsed.clone() {
      let name = candidate.name.trim().to_string();
      let mut profile = ParametricProfile::new(name.clone(), VadType::Media);
      profile.bands = parsed.bands;
      profile.preamp_db = parsed.preamp_db;
      profile.is_custom = false;
      state.output_eq_do(|eq, shm| eq.add_headphone_profile(profile, shm));
      state.headphones_ui.saved_name = Some(name);
      go(state, Step::Done);
      return;
    }
  }
  state.headphones_ui.candidate = Some(candidate);
}

fn step_done(ui: &mut Ui, state: &mut AppState, device_name: &str) {
  let name = state.headphones_ui.saved_name.clone().unwrap_or_default();
  theme::status_line(ui, Severity::Success, &trf("\"{}\" is now used on {}.", &[&name, &device_name]));
  ui.label(tr("Try it: play music you know well and switch \"Headphone EQ\" above off and on to compare."));
  let installed = state.headphones_ui.apo.as_ref().is_some_and(|(c, _)| c.status == ApoStatus::Active);
  if !installed {
    theme::status_line(
      ui,
      Severity::Warning,
      tr(
        "You won't hear a difference until the output EQ is installed on this device (button at the top of this tab).",
      ),
    );
  }
  ui.horizontal(|ui| {
    if ui.button(format!("➕  {}", tr("Add another pair"))).clicked() {
      go(state, Step::Choose);
    }
    if ui.button(tr("Close")).clicked() {
      state.headphones_ui.wizard_open = Some(false);
    }
  });
}

// --- wizard helpers --------------------------------------------------------------------------

fn go(state: &mut AppState, step: Step) {
  state.headphones_ui.step = step;
  state.headphones_ui.downloads_checked = None;
}

fn numbered(ui: &mut Ui, n: u32, text: &str) {
  ui.horizontal_wrapped(|ui| {
    let p = theme::palette(ui);
    // Numbered circle, like the steps in Windows setup pages.
    let (rect, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 11.0, p.accent);
    ui.painter().text(
      rect.center(),
      egui::Align2::CENTER_CENTER,
      n.to_string(),
      egui::FontId::proportional(12.0),
      p.on_accent,
    );
    ui.label(text);
  });
}

fn nav(ui: &mut Ui, state: &mut AppState, back: Step, next: Option<(&str, Step)>) {
  ui.add_space(8.0);
  ui.horizontal(|ui| {
    if ui.button(format!("◀  {}", tr("Back"))).clicked() {
      go(state, back);
    }
    if let Some((label, step)) = next {
      if ui.add(theme::accent_button(ui, &format!("{label}  ▶"))).clicked() {
        go(state, step);
      }
    }
  });
}

fn paste_box(ui: &mut Ui, state: &mut AppState) {
  ui.add(
    egui::TextEdit::multiline(&mut state.headphones_ui.paste_text)
      .desired_rows(3)
      .desired_width(f32::INFINITY)
      .font(egui::TextStyle::Monospace)
      .hint_text("Preamp: -6.4 dB\nFilter 1: ON LSC Fc 105 Hz Gain 6.7 dB Q 0.70\n..."),
  );
  ui.horizontal(|ui| {
    ui.add(
      egui::TextEdit::singleline(&mut state.headphones_ui.paste_name)
        .hint_text(tr("headphone name"))
        .desired_width(200.0),
    );
    let can = !state.headphones_ui.paste_text.trim().is_empty();
    if ui.add_enabled(can, egui::Button::new(tr("Check pasted text"))).clicked() {
      let text = state.headphones_ui.paste_text.clone();
      let name = state.headphones_ui.paste_name.trim().to_string();
      let name = if name.is_empty() { tr("My headphones").to_string() } else { name };
      review_text(state, &text, &name, tr("pasted text"), "", Step::Choose);
    }
  });
}

fn refresh_downloads(ui: &mut Ui, state: &mut AppState) {
  let ui_state = &mut state.headphones_ui;
  if ui_state.downloads_checked.is_none_or(|t| t.elapsed() >= DOWNLOADS_RECHECK) {
    ui_state.downloads_checked = Some(Instant::now());
    ui_state.downloads = autoeq::downloads_dir().map(|d| autoeq::scan_downloads(&d)).unwrap_or_default();
    ui_state.autoeq_found =
      autoeq::candidate_dirs(state.output_eq.settings.autoeq_dir.as_deref()).into_iter().next();
  }
  ui.ctx().request_repaint_after(DOWNLOADS_RECHECK);
}

fn review_file(state: &mut AppState, path: &Path, name: Option<&str>, back: Step) {
  let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
  let default_name =
    file_name.trim_end_matches(".txt").trim_end_matches(" ParametricEQ").trim_end_matches(" FixedBandEQ").to_string();
  match std::fs::read_to_string(path) {
    Ok(text) => review_text(state, &text, name.unwrap_or(&default_name), &path.display().to_string(), &file_name, back),
    Err(e) => {
      let checks = vec![Check { level: CheckLevel::Error, text: trf("Could not read the file: {}", &[&e]) }];
      state.headphones_ui.candidate =
        Some(Candidate { name: default_name, origin: path.display().to_string(), parsed: None, checks });
      state.headphones_ui.review_back = back;
      go(state, Step::Review);
    }
  }
}

fn review_text(state: &mut AppState, text: &str, name: &str, origin: &str, file_name: &str, back: Step) {
  let (parsed, checks) = autoeq::check_file(text, file_name);
  state.headphones_ui.candidate =
    Some(Candidate { name: name.to_string(), origin: origin.to_string(), parsed, checks });
  state.headphones_ui.review_back = back;
  go(state, Step::Review);
}

fn init_ui(state: &mut AppState, device_name: &str) {
  let ui_state = &mut state.headphones_ui;
  if !ui_state.initialized {
    ui_state.initialized = true;
    // A known copy of the database is indexed in the background right away.
    if let Some(dir) = autoeq::candidate_dirs(state.output_eq.settings.autoeq_dir.as_deref()).into_iter().next() {
      ui_state.folder_text = dir.display().to_string();
      start_scan(ui_state, dir);
    }
  }
  // The output device name is a good first query ("Rode NTH-100").
  if ui_state.query_seeded_for.as_deref() != Some(device_name) {
    ui_state.query_seeded_for = Some(device_name.to_string());
    ui_state.query = device_query(device_name);
  }
}

/// "Headphones (Rode NTH-100)" -> "Rode NTH-100"; generic names give an empty query.
fn device_query(device_name: &str) -> String {
  let inner = device_name.split_once('(').map_or(device_name, |(_, rest)| rest.trim_end_matches(')'));
  let generic = ["realtek", "high definition", "audio", "speakers", "headphones", "nvidia", "amd", "usb"];
  let lower = inner.to_lowercase();
  if generic.iter().any(|g| lower.contains(g)) {
    String::new()
  } else {
    inner.trim().to_string()
  }
}

fn start_scan(ui_state: &mut HeadphonesUi, dir: PathBuf) {
  let (tx, rx) = channel();
  ui_state.scanning = Some(rx);
  ui_state.message = Some(trf("Indexing {} ...", &[&dir.display()]));
  std::thread::spawn(move || {
    let entries = autoeq::index_folder(&dir);
    let _ = tx.send((dir, entries));
  });
}

/// Collects a finished background index and remembers the folder.
fn poll_index(ui: &mut Ui, state: &mut AppState) {
  let Some(rx) = state.headphones_ui.scanning.as_ref() else {
    return;
  };
  let Ok((dir, entries)) = rx.try_recv() else {
    ui.ctx().request_repaint_after(Duration::from_millis(200));
    return;
  };
  state.headphones_ui.scanning = None;
  state.headphones_ui.message = Some(if entries.is_empty() {
    trf("No \"... ParametricEQ.txt\" files in {}. Is it the extracted AutoEq folder?", &[&dir.display()])
  } else {
    trf("✔ {} headphone corrections indexed.", &[&entries.len()])
  });
  if !entries.is_empty() && state.output_eq.settings.autoeq_dir.as_ref() != Some(&dir) {
    state.output_eq.settings.autoeq_dir = Some(dir);
    state.output_eq_do(|eq, shm| eq.save_settings(shm));
  }
  state.headphones_ui.entries = entries;
  state.headphones_ui.search_hits = None;
}

/// Is the output APO installed on this output, and is it processing right now?
fn output_apo_status(ui: &mut Ui, state: &mut AppState, device_id: Option<&str>) {
  let stale = state.headphones_ui.apo.as_ref().is_none_or(|(_, t)| t.elapsed() >= APO_RECHECK);
  if stale {
    state.headphones_ui.apo = Some((apo_install::check_flow(Flow::Render, device_id), Instant::now()));
  }
  let live = state.output_apo_live();
  // The service saw sound play on this output while our effect processed nothing.
  let run = super::apo_banner::run_status(state);
  let not_running = run.is_some_and(|r| r.output_not_running.is_some());
  let Some((check, _)) = state.headphones_ui.apo.clone() else {
    return;
  };
  // Only this output's notes: with effects on several devices another one's would mislead.
  let note = run
    .and_then(|r| super::apo_banner::newest_note_on(r, "Output EQ", check.endpoint_guid.as_deref()))
    .map(str::to_string);
  let (text, severity, button) = match check.status {
    ApoStatus::Active if not_running => (
      tr("The output EQ does NOT work on this device while sound plays: you hear it unprocessed.").to_string(),
      Severity::Error,
      None,
    ),
    ApoStatus::Active if live => {
      let (rate, ch) = state.shm_header().map_or((0, 0), |h| {
        (
          h.out_apo_sample_rate.load(std::sync::atomic::Ordering::Relaxed),
          h.out_apo_channels.load(std::sync::atomic::Ordering::Relaxed),
        )
      });
      (
        trf(
          "Output EQ active on this device, processing now ({} kHz, {} ch)",
          &[&format!("{:.1}", rate as f32 / 1000.0), &ch],
        ),
        Severity::Success,
        None,
      )
    }
    ApoStatus::Active => {
      (tr("Output EQ installed on this device (idle: play something to verify)").to_string(), Severity::Success, None)
    }
    ApoStatus::NoMicrophone => (tr("No output device found").to_string(), Severity::Warning, None),
    ApoStatus::NotInstalled => (
      tr(
        "Output EQ is not installed yet: profiles are shown but you don't hear them. It is a Windows audio effect on \
          this device (no virtual sound card).",
      )
      .to_string(),
      Severity::Warning,
      Some(tr("Install")),
    ),
    ApoStatus::Outdated => {
      (tr("The installed output EQ is older than this app.").to_string(), Severity::Warning, Some(tr("Update")))
    }
    ApoStatus::NotOnThisMic => (
      tr("Output EQ is not installed on this output device.").to_string(),
      Severity::Warning,
      Some(tr("Enable on this device")),
    ),
  };
  if severity == Severity::Success {
    theme::status_line(ui, severity, &text);
    ui.add_space(theme::CARD_GAP);
    return;
  }
  theme::info_bar(ui, severity, |ui| {
    ui.label(text);
    if let Some(note) = note.as_deref().filter(|_| not_running) {
      ui.label(theme::caption(trf("Last report from Windows audio: {}", &[&note])));
    }
    if let (Some(label), Some(_)) = (button, check.endpoint_guid.as_deref()) {
      if !check.can_install {
        ui.label(theme::caption(tr("(installer files not found next to the app)")));
      } else if ui
        .add(theme::accent_button(ui, label))
        .on_hover_text(tr(
          "Asks for administrator rights. Windows audio restarts for a few seconds; no virtual device is created.",
        ))
        .clicked()
      {
        // The setup card explains the prompt and adds any missing connected devices in one go.
        super::setup::open_for_output(state);
        state.headphones_ui.apo = None;
      }
    }
  });
  ui.add_space(theme::CARD_GAP);
}

#[cfg(test)]
mod tests {
  use super::device_query;

  #[test]
  fn device_names_become_search_queries() {
    assert_eq!(device_query("Headphones (Rode NTH-100)"), "Rode NTH-100");
    assert_eq!(device_query("Realtek HD Audio 2nd output (Realtek(R) Audio)"), "");
    assert_eq!(device_query("Sluchátka (Jabra Evolve2 65)"), "Jabra Evolve2 65");
  }
}
