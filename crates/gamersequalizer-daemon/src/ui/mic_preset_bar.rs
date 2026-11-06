//! Preset row of the microphone EQ: pick, save / save as, reset, delete, share (copy as text)
//! and import (paste text). Switching away from unsaved changes asks whether to keep them.

use egui::{Align2, Color32, RichText, Ui};

use super::mic_presets::{self, MicPreset, NameCheck, PresetSource};
use super::state::AppState;
use super::theme::{self, Severity};
use crate::i18n::{tr, trf};
use gamersequalizer_common::profile::{ParametricProfile, VadType};

#[derive(Default)]
pub struct MicPresetUi {
  dialog: Dialog,
  /// Name field of the save dialog.
  name: String,
  /// Text field of the import dialog.
  import_text: String,
  /// Result of the last action: (ok, text).
  message: Option<(bool, String)>,
}

#[derive(Default)]
enum Dialog {
  #[default]
  None,
  /// `switch_to`: the preset picked while the current EQ had unsaved changes.
  Save {
    switch_to: Option<ParametricProfile>,
  },
  Delete,
  Import,
}

pub fn render_preset_bar(ui: &mut Ui, state: &mut AppState) {
  let current = state.mic_profile.name.clone();
  let modified = mic_presets::is_modified(&state.available_mic_profiles, &state.mic_profile);
  let source = mic_presets::find(&state.available_mic_profiles, &current).map(|p| p.source);

  ui.horizontal_wrapped(|ui| {
    ui.label(tr("Preset"));
    let shown = if modified { trf("{}  • modified", &[&tr(&current)]) } else { tr(&current).to_string() };
    let mut picked = None;
    let color = if modified { theme::palette(ui).warn } else { Color32::PLACEHOLDER };
    egui::ComboBox::from_id_salt("combo_mic_profile")
      .selected_text(RichText::new(shown).color(color))
      .width(260.0)
      .show_ui(ui, |ui| picked = preset_list(ui, &state.available_mic_profiles, &current));
    if let Some(p) = picked {
      if modified {
        state.mic_preset_ui.name = suggested_name(state);
        state.mic_preset_ui.dialog = Dialog::Save { switch_to: Some(p) };
      } else {
        select(state, p);
      }
    }

    let can_save = modified && source == Some(PresetSource::User);
    if ui
      .add_enabled(can_save, egui::Button::new(format!("💾  {}", tr("Save"))))
      .on_hover_text(tr("Save the changes into this preset."))
      .on_disabled_hover_text(tr("Only your own presets can be saved over; use \"Save as…\" for this one."))
      .clicked()
    {
      save_as(state, &current, None);
    }
    if ui.button(tr("Save as…")).on_hover_text(tr("Save the current EQ as your own preset (listed first).")).clicked()
    {
      state.mic_preset_ui.name = suggested_name(state);
      state.mic_preset_ui.dialog = Dialog::Save { switch_to: None };
    }
    if modified
      && source.is_some()
      && ui
        .button(format!("↺  {}", tr("Reset")))
        .on_hover_text(tr("Throw away the changes, back to the preset."))
        .clicked()
    {
      if let Some(p) = mic_presets::find(&state.available_mic_profiles, &current) {
        let p = p.profile.clone();
        select(state, p);
      }
    }
    if source == Some(PresetSource::User) && ui.button("🗑").on_hover_text(tr("Delete this preset")).clicked() {
      state.mic_preset_ui.dialog = Dialog::Delete;
    }
    ui.add_space(12.0);
    if ui
      .button(format!("📋  {}", tr("Copy to share")))
      .on_hover_text(tr("Copies the EQ as text: paste it into a chat or pastebin.com. Works in Equalizer APO too."))
      .clicked()
    {
      ui.ctx().copy_text(mic_presets::to_share_text(&state.mic_profile));
      state.mic_preset_ui.message = Some((
        true,
        tr("Copied. Paste it anywhere (chat, pastebin.com); the other side uses \"Import…\".").to_string(),
      ));
    }
    if ui
      .button(format!("📥  {}", tr("Import…")))
      .on_hover_text(tr("Paste an EQ someone shared (or Equalizer APO / AutoEQ text)."))
      .clicked()
    {
      state.mic_preset_ui.import_text.clear();
      state.mic_preset_ui.dialog = Dialog::Import;
    }
  });
  if let Some((ok, msg)) = state.mic_preset_ui.message.clone() {
    theme::status_line(ui, if ok { Severity::Success } else { Severity::Error }, &msg);
  }

  dialogs(ui.ctx(), state);
}

/// Grouped list; returns the clicked preset.
fn preset_list(ui: &mut Ui, list: &[MicPreset], current: &str) -> Option<ParametricProfile> {
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

fn dialogs(ctx: &egui::Context, state: &mut AppState) {
  match std::mem::take(&mut state.mic_preset_ui.dialog) {
    Dialog::None => {}
    Dialog::Save { switch_to } => save_dialog(ctx, state, switch_to),
    Dialog::Delete => delete_dialog(ctx, state),
    Dialog::Import => import_dialog(ctx, state),
  }
}

/// Centered, fixed dialog window. Each dialog puts itself back unless a button closed it.
fn window(title: &str) -> egui::Window<'_> {
  egui::Window::new(title)
    .collapsible(false)
    .resizable(false)
    .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
    .default_width(440.0)
}

fn save_dialog(ctx: &egui::Context, state: &mut AppState, switch_to: Option<ParametricProfile>) {
  let mut keep_open = true;
  let title = if switch_to.is_some() { tr("Keep your changes?") } else { tr("Save preset") };
  window(title).show(ctx, |ui| {
    if switch_to.is_some() {
      ui.label(trf(
        "\"{}\" has changes that are not saved in any preset. Save them as your own preset before switching?",
        &[&tr(&state.mic_profile.name)],
      ));
    }
    ui.horizontal(|ui| {
      ui.label(tr("Name:"));
      ui.text_edit_singleline(&mut state.mic_preset_ui.name).request_focus();
    });
    let check = mic_presets::check_new_name(&state.available_mic_profiles, &state.mic_preset_ui.name);
    let (can, hint) = match &check {
      NameCheck::Ok => (true, None),
      NameCheck::Empty => (false, Some(tr("Type a name."))),
      NameCheck::Overwrite => (true, Some(tr("You already have a preset with this name; saving replaces it."))),
      NameCheck::Taken(PresetSource::Builtin) => (false, Some(tr("A built-in preset has this name; choose another."))),
      NameCheck::Taken(_) => (false, Some(tr("A Sonar preset has this name; choose another."))),
    };
    if let Some(h) = hint {
      theme::status_line(ui, if can { Severity::Warning } else { Severity::Error }, h);
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
      let label = match (check == NameCheck::Overwrite, switch_to.is_some()) {
        (true, true) => tr("Replace and switch"),
        (true, false) => tr("Replace"),
        (false, true) => tr("Save and switch"),
        (false, false) => tr("Save"),
      };
      let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
      if ui.add_enabled(can, theme::accent_button(ui, label)).clicked() || (can && enter) {
        let name = state.mic_preset_ui.name.trim().to_string();
        save_as(state, &name, switch_to.clone());
        keep_open = false;
      }
      if let Some(p) = switch_to.as_ref() {
        if ui.button(tr("Switch without saving")).clicked() {
          select(state, p.clone());
          keep_open = false;
        }
      }
      if ui.button(tr("Cancel")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        keep_open = false;
      }
    });
  });
  if keep_open {
    state.mic_preset_ui.dialog = Dialog::Save { switch_to };
  }
}

fn delete_dialog(ctx: &egui::Context, state: &mut AppState) {
  let mut keep_open = true;
  let name = state.mic_profile.name.clone();
  window(tr("Delete preset")).show(ctx, |ui| {
    ui.label(trf("Delete your preset \"{}\"? The EQ you hear now stays as it is.", &[&name]));
    ui.add_space(8.0);
    ui.horizontal(|ui| {
      if ui.add(theme::accent_button(ui, tr("Delete"))).clicked() {
        state.mic_preset_ui.message = Some(match mic_presets::delete_user_preset(&state.storage, &name) {
          Ok(()) => (true, trf("Deleted \"{}\".", &[&name])),
          Err(e) => {
            log::warn!("[MicPresets] delete {name} failed: {e}");
            (false, trf("Could not delete \"{}\": {}", &[&name, &e]))
          }
        });
        reload(state);
        keep_open = false;
      }
      if ui.button(tr("Cancel")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        keep_open = false;
      }
    });
  });
  if keep_open {
    state.mic_preset_ui.dialog = Dialog::Delete;
  }
}

fn import_dialog(ctx: &egui::Context, state: &mut AppState) {
  let mut keep_open = true;
  window(tr("Import a shared EQ")).show(ctx, |ui| {
    ui.label(tr("Paste the text someone shared (or Equalizer APO / AutoEQ filter lines):"));
    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
      ui.add(
        egui::TextEdit::multiline(&mut state.mic_preset_ui.import_text)
          .desired_rows(8)
          .desired_width(480.0)
          .font(egui::TextStyle::Monospace)
          .hint_text("# GamersEqualizer microphone preset\n# Name: ...\nFilter 1: ON HPQ Fc 80.0 Hz Q 0.541\n..."),
      );
    });
    let parsed = mic_presets::from_share_text(&state.mic_preset_ui.import_text);
    match &parsed {
      Ok(p) => {
        let name = p.name.as_deref().unwrap_or(tr("(no name, you choose one next)"));
        theme::status_line(ui, Severity::Success, &trf("{} filters, name: {}", &[&p.bands.len(), &name]));
        for note in &p.notes {
          theme::status_line(ui, Severity::Warning, note);
        }
      }
      Err(e) if !state.mic_preset_ui.import_text.trim().is_empty() => {
        theme::status_line(ui, Severity::Error, e);
      }
      Err(_) => {}
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
      let ok = parsed.as_ref().ok();
      if ui.add_enabled(ok.is_some(), theme::accent_button(ui, tr("Import"))).clicked() {
        if let Some(p) = ok {
          // The pasted EQ becomes the current one; saving it under a name is the next step.
          let mut profile = ParametricProfile::new(p.name.clone().unwrap_or_default(), VadType::Mic);
          profile.bands = p.bands.clone();
          state.mic_preset_ui.name = unique_name(state, p.name.as_deref().unwrap_or(tr("Imported")));
          state.mic_profile = profile;
          state.apply_mic_eq();
          state.mic_preset_ui.dialog = Dialog::Save { switch_to: None };
          return;
        }
      }
      if ui.button(tr("Cancel")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        keep_open = false;
      }
    });
    if matches!(state.mic_preset_ui.dialog, Dialog::Save { .. }) {
      keep_open = false;
    }
  });
  if keep_open {
    state.mic_preset_ui.dialog = Dialog::Import;
  }
}

/// Saves the current EQ as the user's preset `name`, makes it current, then switches if asked.
fn save_as(state: &mut AppState, name: &str, switch_to: Option<ParametricProfile>) {
  let mut profile = state.mic_profile.clone();
  profile.name = name.to_string();
  match mic_presets::save_user_preset(&state.storage, &profile) {
    Ok(()) => {
      state.mic_profile.name = name.to_string();
      state.apply_mic_eq();
      state.mic_preset_ui.message = Some((true, trf("Saved as \"{}\".", &[&name])));
      reload(state);
      if let Some(p) = switch_to {
        select(state, p);
      }
    }
    Err(e) => {
      // Not switching: the changes would be lost.
      log::warn!("[MicPresets] save {name} failed: {e}");
      state.mic_preset_ui.message = Some((false, trf("Could not save \"{}\": {}", &[&name, &e])));
    }
  }
}

fn select(state: &mut AppState, profile: ParametricProfile) {
  state.mic_profile = profile;
  state.apply_mic_eq();
}

fn reload(state: &mut AppState) {
  state.available_mic_profiles = mic_presets::all_mic_presets(&state.storage);
}

/// "Male voice" -> "Male voice (mine)"; own presets keep their name (save over).
fn suggested_name(state: &AppState) -> String {
  let current = &state.mic_profile.name;
  match mic_presets::find(&state.available_mic_profiles, current) {
    Some(p) if p.source == PresetSource::User => current.clone(),
    _ if current.trim().is_empty() => unique_name(state, tr("My preset")),
    _ => unique_name(state, &trf("{} (mine)", &[&tr(current)])),
  }
}

fn unique_name(state: &AppState, base: &str) -> String {
  let free = |n: &str| mic_presets::check_new_name(&state.available_mic_profiles, n) == NameCheck::Ok;
  if free(base) {
    return base.to_string();
  }
  (2..100).map(|i| format!("{base} {i}")).find(|n| free(n)).unwrap_or_else(|| base.to_string())
}
