//! Unified EQ profile bar and dialogs shared across Headphones, Game, Chat, Media and Mic:
//! New, Save as, Delete, Copy to share, Import from Equalizer APO / AutoEQ text, plus
//! Mic's Save / Reset for unsaved edits over read-only presets.

use egui::{Align2, Color32, RichText, Ui};

use super::mic_presets::{self, MicPreset, NameCheck, PresetSource};
use super::state::{AppState, OutputLayer};
use super::theme::{self, Severity};
use crate::i18n::{tr, trf};
use crate::output_eq::same_profile_slot;
use crate::output_eq::share_text::{self, SHARE_HEADER, SHARE_NAME_PREFIX};
use crate::sonar_importer::SonarDbReader;
use gamersequalizer_common::biquad::{MAX_EQ_BANDS, MAX_LAYER_EQ_BANDS};
use gamersequalizer_common::profile::{BiquadBand, ParametricProfile, VadType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqTarget {
  Output(OutputLayer),
  Mic,
}

impl EqTarget {
  fn max_slots(self) -> usize {
    match self {
      EqTarget::Mic => MAX_EQ_BANDS,
      EqTarget::Output(_) => MAX_LAYER_EQ_BANDS,
    }
  }
}

#[derive(Default)]
pub struct ProfileBarUi {
  dialog: Option<(EqTarget, Dialog)>,
  /// Name field of the New / Save as / Save dialog.
  name: String,
  /// Focus the name field once when a dialog opens.
  focus_name: bool,
  /// Text field of the Import dialog.
  import_text: String,
  /// Result of the last action for one target: (target, ok, text).
  notice: Option<(EqTarget, bool, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SaveMode {
  New,
  SaveAs,
  Import,
  KeepOnSwitch,
}

/// The EQ a Save dialog is about to write, apart from its name.
#[derive(Clone)]
struct SaveDraft {
  bands: Vec<BiquadBand>,
  gain_db: f32,
  preamp_db: f32,
  /// Mic only: preset picked while the current EQ had unsaved changes.
  switch_to: Option<ParametricProfile>,
}

impl SaveDraft {
  fn of(p: &ParametricProfile) -> Self {
    Self { bands: p.bands.clone(), gain_db: p.gain_db, preamp_db: p.preamp_db, switch_to: None }
  }

  fn flat(bands: Vec<BiquadBand>) -> Self {
    Self { bands, gain_db: 0.0, preamp_db: 0.0, switch_to: None }
  }

  fn profile(&self, name: &str, vad: VadType) -> ParametricProfile {
    let mut p = ParametricProfile::new(name, vad);
    p.bands = self.bands.clone();
    p.gain_db = self.gain_db;
    p.preamp_db = self.preamp_db;
    p
  }
}

enum Dialog {
  Save {
    mode: SaveMode,
    draft: SaveDraft,
  },
  Delete {
    name: String,
    /// Output only: mention "Import from Sonar DB" (checked once when the dialog opens).
    sonar_hint: bool,
  },
  Import,
}

enum NameStatus<'a> {
  Ok,
  Empty,
  Overwrite(&'a str),
  Taken(&'a str),
}

/// Microphone preset combo + shared actions + notice + dialogs.
pub fn render_mic_bar(ui: &mut Ui, state: &mut AppState) {
  let current = state.mic_profile.name.clone();
  let modified = mic_presets::is_modified(&state.available_mic_profiles, &state.mic_profile);

  ui.horizontal_wrapped(|ui| {
    ui.label(tr("Preset"));
    let shown = if modified { trf("{}  • modified", &[&tr(&current)]) } else { tr(&current).to_string() };
    let mut picked = None;
    let color = if modified { theme::palette(ui).warn } else { Color32::PLACEHOLDER };
    egui::ComboBox::from_id_salt("combo_mic_profile")
      .selected_text(RichText::new(shown).color(color))
      .width(260.0)
      .show_ui(ui, |ui| picked = mic_preset_list(ui, &state.available_mic_profiles, &current));
    if let Some(p) = picked {
      if modified {
        let draft = SaveDraft { switch_to: Some(p), ..SaveDraft::of(&state.mic_profile) };
        let name = suggested_mic_name(state, false);
        open_save_dialog(state, EqTarget::Mic, SaveMode::KeepOnSwitch, name, draft);
      } else {
        state.use_mic_profile(p);
      }
    }

    action_buttons(ui, state, EqTarget::Mic, modified);
  });

  render_notice_and_dialogs(ui, state, EqTarget::Mic);
}

/// Shared profile action buttons (`New…`, `Save as…`, `Delete`, `Copy to share`, `Import…`)
/// for an output EQ layer, placed inside the caller's horizontal row.
pub fn render_output_buttons(ui: &mut Ui, state: &mut AppState, layer: OutputLayer) {
  action_buttons(ui, state, EqTarget::Output(layer), false);
}

/// Status notice + open modal dialogs for an output EQ layer.
pub fn render_output_notice(ui: &mut Ui, state: &mut AppState, layer: OutputLayer) {
  render_notice_and_dialogs(ui, state, EqTarget::Output(layer));
}

/// `modified`: Mic only, the current EQ differs from its preset.
fn action_buttons(ui: &mut Ui, state: &mut AppState, target: EqTarget, modified: bool) {
  let has_profile = active_profile(state, target).is_some();

  if ui.button(format!("➕  {}", tr("New…"))).on_hover_text(tr("Create a new flat profile.")).clicked() {
    let name = unique_name(state, target, tr("New profile"));
    open_save_dialog(state, target, SaveMode::New, name, SaveDraft::flat(Vec::new()));
  }

  let mic_source = match target {
    EqTarget::Mic => mic_presets::find(&state.available_mic_profiles, &state.mic_profile.name).map(|p| p.source),
    EqTarget::Output(_) => None,
  };

  if target == EqTarget::Mic {
    let can_save = modified && mic_source == Some(PresetSource::User);
    if ui
      .add_enabled(can_save, egui::Button::new(format!("💾  {}", tr("Save"))))
      .on_hover_text(tr("Save the changes into this preset."))
      .on_disabled_hover_text(tr("Only your own presets can be saved over; use \"Save as…\" for this one."))
      .clicked()
    {
      let (name, draft) = (state.mic_profile.name.clone(), SaveDraft::of(&state.mic_profile));
      save_mic_preset(state, &name, draft, false);
    }
  }

  if ui
    .add_enabled(has_profile, egui::Button::new(tr("Save as…")))
    .on_hover_text(tr("Save a copy of this profile under a new name."))
    .on_disabled_hover_text(tr("Select a profile first."))
    .clicked()
  {
    if let Some(p) = active_profile(state, target).cloned() {
      let suggested = match target {
        EqTarget::Mic => suggested_mic_name(state, true),
        EqTarget::Output(_) => unique_name(state, target, &trf("{} (copy)", &[&tr(&p.name)])),
      };
      open_save_dialog(state, target, SaveMode::SaveAs, suggested, SaveDraft::of(&p));
    }
  }

  if target == EqTarget::Mic
    && modified
    && mic_source.is_some()
    && ui
      .button(format!("↺  {}", tr("Reset")))
      .on_hover_text(tr("Throw away the changes, back to the preset."))
      .clicked()
  {
    if let Some(p) = mic_presets::find(&state.available_mic_profiles, &state.mic_profile.name) {
      let p = p.profile.clone();
      state.use_mic_profile(p);
    }
  }

  let (can_delete, delete_disabled_hint) = match target {
    EqTarget::Mic => (mic_source == Some(PresetSource::User), tr("Only your own presets can be deleted.")),
    EqTarget::Output(_) => (has_profile, tr("Select a profile first.")),
  };
  if ui
    .add_enabled(can_delete, egui::Button::new("🗑"))
    .on_hover_text(tr("Delete this profile"))
    .on_disabled_hover_text(delete_disabled_hint)
    .clicked()
  {
    if let Some(name) = active_profile(state, target).map(|p| p.name.clone()) {
      let sonar_hint =
        matches!(target, EqTarget::Output(l) if l != OutputLayer::Headphone) && SonarDbReader::default_db_exists();
      state.profile_bar.dialog = Some((target, Dialog::Delete { name, sonar_hint }));
      state.profile_bar.notice = None;
    }
  }

  ui.add_space(12.0);

  if ui
    .add_enabled(has_profile, egui::Button::new(format!("📋  {}", tr("Copy to share"))))
    .on_hover_text(tr("Copies the EQ as text: paste it into a chat or pastebin.com. Works in Equalizer APO too."))
    .on_disabled_hover_text(tr("Select a profile first."))
    .clicked()
  {
    if let Some(text) = active_profile(state, target).map(share_text::to_share_text) {
      ui.ctx().copy_text(text);
      state.profile_bar.notice = Some((
        target,
        true,
        tr("Copied. Paste it anywhere (chat, pastebin.com); the other side uses \"Import…\".").to_string(),
      ));
    }
  }

  if ui
    .button(format!("📥  {}", tr("Import…")))
    .on_hover_text(tr("Paste an EQ someone shared (or Equalizer APO / AutoEQ text)."))
    .clicked()
  {
    state.profile_bar.import_text.clear();
    state.profile_bar.focus_name = true;
    state.profile_bar.dialog = Some((target, Dialog::Import));
  }
}

fn render_notice_and_dialogs(ui: &mut Ui, state: &mut AppState, target: EqTarget) {
  if let Some((notice_target, ok, msg)) = state.profile_bar.notice.clone() {
    if notice_target == target {
      theme::status_line(ui, if ok { Severity::Success } else { Severity::Error }, &msg);
    }
  }

  let Some((dlg_target, dlg)) = state.profile_bar.dialog.take() else {
    return;
  };
  if dlg_target != target {
    state.profile_bar.dialog = Some((dlg_target, dlg));
    return;
  }
  match dlg {
    Dialog::Save { mode, draft } => save_dialog(ui.ctx(), state, target, mode, draft),
    Dialog::Delete { name, sonar_hint } => delete_dialog(ui.ctx(), state, target, name, sonar_hint),
    Dialog::Import => import_dialog(ui.ctx(), state, target),
  }
}

/// Grouped list for Mic; returns the clicked preset.
fn mic_preset_list(ui: &mut Ui, list: &[MicPreset], current: &str) -> Option<ParametricProfile> {
  let mut picked = None;
  let mut last = None;
  for p in list {
    if last != Some(p.source) {
      if last.is_some() {
        ui.separator();
      }
      let title = match p.source {
        PresetSource::User => tr("My presets"),
        PresetSource::Builtin => tr("Built-in"),
        PresetSource::Sonar => tr("Imported from Sonar"),
      };
      ui.label(theme::caption(title));
      last = Some(p.source);
    }
    // Built-in names are translation keys; user and Sonar names pass through unchanged.
    if ui.selectable_label(p.profile.name == current, tr(&p.profile.name)).clicked() {
      picked = Some(p.profile.clone());
    }
  }
  picked
}

/// Centered, fixed dialog window. Each dialog puts itself back unless a button closed it.
fn window(title: &str) -> egui::Window<'_> {
  egui::Window::new(title)
    .collapsible(false)
    .resizable(false)
    .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
    .default_width(440.0)
}

fn save_dialog(ctx: &egui::Context, state: &mut AppState, target: EqTarget, mode: SaveMode, draft: SaveDraft) {
  let mut keep_open = true;
  let title = match mode {
    SaveMode::New => tr("New profile"),
    SaveMode::SaveAs => tr("Save profile as"),
    SaveMode::Import => tr("Save imported profile"),
    SaveMode::KeepOnSwitch => tr("Keep your changes?"),
  };
  let mic_discard_warn = target == EqTarget::Mic
    && matches!(mode, SaveMode::New | SaveMode::Import)
    && mic_presets::is_modified(&state.available_mic_profiles, &state.mic_profile);

  window(title).show(ctx, |ui| {
    if mode == SaveMode::KeepOnSwitch {
      ui.label(trf(
        "\"{}\" has changes that are not saved in any preset. Save them as your own preset before switching?",
        &[&tr(&state.mic_profile.name)],
      ));
    } else if mic_discard_warn {
      theme::status_line(ui, Severity::Warning, tr("Unsaved changes to the current EQ will be discarded."));
    }
    ui.horizontal(|ui| {
      ui.label(tr("Name:"));
      let edit = ui.text_edit_singleline(&mut state.profile_bar.name);
      if std::mem::take(&mut state.profile_bar.focus_name) {
        edit.request_focus();
      }
    });

    let (can, hint, overwrite) = match check_name(state, target, &state.profile_bar.name) {
      NameStatus::Ok => (true, None, false),
      NameStatus::Empty => (false, Some(tr("Type a name.")), false),
      NameStatus::Overwrite(msg) => (true, Some(msg), true),
      NameStatus::Taken(msg) => (false, Some(msg), false),
    };
    if let Some(h) = hint {
      theme::status_line(ui, if can { Severity::Warning } else { Severity::Error }, h);
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
      let label = match (overwrite, draft.switch_to.is_some(), mode == SaveMode::New) {
        (true, true, _) => tr("Replace and switch"),
        (true, false, _) => tr("Replace"),
        (false, true, _) => tr("Save and switch"),
        (false, false, true) => tr("Create"),
        (false, false, false) => tr("Save"),
      };
      let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
      if ui.add_enabled(can, theme::accent_button(ui, label)).clicked() || (can && enter) {
        let name = state.profile_bar.name.trim().to_string();
        commit_save(state, target, mode, &name, draft.clone());
        keep_open = false;
      }
      if let Some(p) = draft.switch_to.as_ref() {
        if ui.button(tr("Switch without saving")).clicked() {
          state.use_mic_profile(p.clone());
          keep_open = false;
        }
      }
      if ui.button(tr("Cancel")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        keep_open = false;
      }
    });
  });
  if keep_open && state.profile_bar.dialog.is_none() {
    state.profile_bar.dialog = Some((target, Dialog::Save { mode, draft }));
  }
}

fn delete_dialog(ctx: &egui::Context, state: &mut AppState, target: EqTarget, name: String, sonar_hint: bool) {
  let mut keep_open = true;
  let title = if target == EqTarget::Mic { tr("Delete preset") } else { tr("Delete profile") };
  window(title).show(ctx, |ui| {
    let prompt = match target {
      EqTarget::Mic => trf("Delete your preset \"{}\"? The EQ you hear now stays as it is.", &[&name]),
      EqTarget::Output(_) => {
        let mut text =
          trf("Delete \"{}\"? The file moves to profiles\\_deleted and can be restored from there.", &[&tr(&name)]);
        if sonar_hint {
          text.push(' ');
          text.push_str(tr("\"Import from Sonar DB\" brings Sonar presets back."));
        }
        text
      }
    };
    ui.label(prompt);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
      if ui.add(theme::accent_button(ui, tr("Delete"))).clicked() {
        commit_delete(state, target, &name);
        keep_open = false;
      }
      if ui.button(tr("Cancel")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        keep_open = false;
      }
    });
  });
  if keep_open {
    state.profile_bar.dialog = Some((target, Dialog::Delete { name, sonar_hint }));
  }
}

fn import_dialog(ctx: &egui::Context, state: &mut AppState, target: EqTarget) {
  let mut keep_open = true;
  window(tr("Import a shared EQ")).show(ctx, |ui| {
    ui.label(tr("Paste the text someone shared (or Equalizer APO / AutoEQ filter lines):"));
    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
      let edit = ui.add(
        egui::TextEdit::multiline(&mut state.profile_bar.import_text)
          .desired_rows(8)
          .desired_width(480.0)
          .font(egui::TextStyle::Monospace)
          .hint_text(format!(
            "{SHARE_HEADER}\n{SHARE_NAME_PREFIX} ...\nFilter 1: ON PK Fc 1000 Hz Gain 3.0 dB Q 1.4\n..."
          )),
      );
      if std::mem::take(&mut state.profile_bar.focus_name) {
        edit.request_focus();
      }
    });
    let parsed = share_text::from_share_text(&state.profile_bar.import_text, target.max_slots());
    match &parsed {
      Ok(p) => {
        let name = p.name.as_deref().unwrap_or(tr("(no name, you choose one next)"));
        theme::status_line(ui, Severity::Success, &trf("{} filters, name: {}", &[&p.bands.len(), &name]));
        for note in &p.notes {
          theme::status_line(ui, Severity::Warning, note);
        }
      }
      Err(e) if !state.profile_bar.import_text.trim().is_empty() => {
        theme::status_line(ui, Severity::Error, e);
      }
      Err(_) => {}
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
      let ok = parsed.as_ref().ok();
      if ui.add_enabled(ok.is_some(), theme::accent_button(ui, tr("Import"))).clicked() {
        if let Some(p) = ok {
          let suggested = unique_name(state, target, p.name.as_deref().unwrap_or(tr("Imported")));
          open_save_dialog(state, target, SaveMode::Import, suggested, SaveDraft::flat(p.bands.clone()));
          keep_open = false;
          return;
        }
      }
      if ui.button(tr("Cancel")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        keep_open = false;
      }
    });
  });
  if keep_open && state.profile_bar.dialog.is_none() {
    state.profile_bar.dialog = Some((target, Dialog::Import));
  }
}

fn open_save_dialog(state: &mut AppState, target: EqTarget, mode: SaveMode, name: String, draft: SaveDraft) {
  state.profile_bar.name = name;
  state.profile_bar.focus_name = true;
  state.profile_bar.dialog = Some((target, Dialog::Save { mode, draft }));
}

fn active_profile(state: &AppState, target: EqTarget) -> Option<&ParametricProfile> {
  match target {
    EqTarget::Mic => Some(&state.mic_profile),
    EqTarget::Output(layer) => state.output_eq.layer_profile(layer),
  }
}

fn check_name<'a>(state: &AppState, target: EqTarget, name: &str) -> NameStatus<'a> {
  let trimmed = name.trim();
  if trimmed.is_empty() {
    return NameStatus::Empty;
  }
  match target {
    EqTarget::Mic => match mic_presets::check_new_name(&state.available_mic_profiles, trimmed) {
      NameCheck::Ok => NameStatus::Ok,
      NameCheck::Empty => NameStatus::Empty,
      NameCheck::Overwrite => {
        NameStatus::Overwrite(tr("You already have a preset with this name; saving replaces it."))
      }
      NameCheck::Taken(PresetSource::Builtin) => {
        NameStatus::Taken(tr("A built-in preset has this name; choose another."))
      }
      NameCheck::Taken(_) => NameStatus::Taken(tr("A Sonar preset has this name; choose another.")),
    },
    EqTarget::Output(layer) => {
      if state.output_eq.layer_profile_exists(layer, trimmed) {
        NameStatus::Overwrite(tr("A profile with this name exists and will be replaced."))
      } else {
        NameStatus::Ok
      }
    }
  }
}

fn commit_save(state: &mut AppState, target: EqTarget, mode: SaveMode, name: &str, draft: SaveDraft) {
  match target {
    EqTarget::Mic => save_mic_preset(state, name, draft, mode == SaveMode::New),
    EqTarget::Output(layer) => {
      if let Some(unsaved) = state.out_eq_unsaved.take() {
        state.output_eq_do(|eq, shm| eq.save_layer_profile(unsaved, shm));
      }
      let profile = draft.profile(name, layer.vad().unwrap_or(VadType::Media));
      let result = state.output_eq_do(|eq, shm| eq.add_layer_profile(layer, profile, shm));
      state.profile_bar.notice = Some(match result {
        // A detected game keeps its profile: the new one is saved but not shown in the editor.
        Ok(())
          if layer == OutputLayer::Game
            && !state.output_eq.layer_profile_name(layer).is_some_and(|n| same_profile_slot(n, name)) =>
        {
          (
            target,
            true,
            trf("Saved \"{}\". The detected game keeps its own profile: pick the new one above to use it.", &[&name]),
          )
        }
        Ok(()) => {
          let msg =
            if mode == SaveMode::New { trf("Created \"{}\".", &[&name]) } else { trf("Saved as \"{}\".", &[&name]) };
          (target, true, msg)
        }
        Err(e) => {
          log::warn!("[OutputEq] save {name} failed: {e}");
          (target, false, trf("Could not save \"{}\": {}", &[&name, &e]))
        }
      });
    }
  }
}

fn save_mic_preset(state: &mut AppState, name: &str, draft: SaveDraft, created: bool) {
  let switch_to = draft.switch_to.clone();
  match state.save_mic_preset(draft.profile(name, VadType::Mic)) {
    Ok(()) => {
      let msg = if created { trf("Created \"{}\".", &[&name]) } else { trf("Saved as \"{}\".", &[&name]) };
      state.profile_bar.notice = Some((EqTarget::Mic, true, msg));
      if let Some(p) = switch_to {
        state.use_mic_profile(p);
      }
    }
    Err(e) => {
      // Not switching: the changes would be lost.
      log::warn!("[MicPresets] save {name} failed: {e}");
      state.profile_bar.notice = Some((EqTarget::Mic, false, trf("Could not save \"{}\": {}", &[&name, &e])));
    }
  }
}

fn commit_delete(state: &mut AppState, target: EqTarget, name: &str) {
  state.profile_bar.notice = Some(match target {
    EqTarget::Mic => match state.delete_mic_preset(name) {
      Ok(()) => (target, true, trf("Deleted \"{}\".", &[&name])),
      Err(e) => {
        log::warn!("[MicPresets] delete {name} failed: {e}");
        (target, false, trf("Could not delete \"{}\": {}", &[&name, &e]))
      }
    },
    EqTarget::Output(layer) => {
      if state.out_eq_unsaved == Some(layer) {
        state.out_eq_unsaved = None;
      }
      let result = state.output_eq_do(|eq, shm| eq.delete_layer_profile(layer, name, shm));
      match result {
        Ok(path) => (target, true, trf("Deleted \"{}\" (moved to {}).", &[&name, &path.display()])),
        Err(e) => (target, false, trf("Could not delete \"{}\": {}", &[&name, &e])),
      }
    }
  });
}

/// For Mic: built-in/Sonar -> "<name> (mine)", user -> "<name> (copy)" (when copying explicitly)
/// or keep "<name>" (when prompted on switch).
fn suggested_mic_name(state: &AppState, as_copy: bool) -> String {
  let current = &state.mic_profile.name;
  match mic_presets::find(&state.available_mic_profiles, current) {
    Some(p) if p.source == PresetSource::User && !as_copy => current.clone(),
    Some(p) if p.source == PresetSource::User => unique_name(state, EqTarget::Mic, &trf("{} (copy)", &[&tr(current)])),
    _ if current.trim().is_empty() => unique_name(state, EqTarget::Mic, tr("My preset")),
    _ => unique_name(state, EqTarget::Mic, &trf("{} (mine)", &[&tr(current)])),
  }
}

fn unique_name(state: &AppState, target: EqTarget, base: &str) -> String {
  let free = |n: &str| matches!(check_name(state, target, n), NameStatus::Ok);
  if free(base) {
    return base.to_string();
  }
  (2..100).map(|i| format!("{base} {i}")).find(|n| free(n)).unwrap_or_else(|| base.to_string())
}
