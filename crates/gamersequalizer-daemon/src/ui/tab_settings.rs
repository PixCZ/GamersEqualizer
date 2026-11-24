//! Settings page: appearance (light/dark), language, start with Windows, the window /
//! service actions that used to sit at the bottom of the navigation pane, and uninstall.

use super::prefs::ThemePref;
use super::state::AppState;
use super::theme::{self, Severity, icons};
use crate::i18n::{self, LANGUAGES, LanguagePref, tr, trf};
use crate::settings_transfer::{self, Part, Selection, Summary};
use crate::win_audio::shell;
use egui::{RichText, Ui};

/// Uninstall card: asks once more before doing anything.
#[derive(Default)]
pub struct UninstallUi {
  confirming: bool,
  remove_user_data: bool,
  error: Option<String>,
}

/// Export / import card. Each list holds the parts with their checkbox.
#[derive(Default)]
pub struct TransferUi {
  /// Export started: which parts go into the file.
  export: Option<Vec<(Part, bool)>>,
  /// File picked for import, waiting for confirmation.
  pending: Option<(Summary, Vec<(Part, bool)>)>,
  message: Option<(Severity, String)>,
}

impl TransferUi {
  /// The app was started again after an import: "ok" or the error text.
  pub fn show_import_result(&mut self, result: &str) {
    self.message = Some(if result == "ok" {
      (
        Severity::Success,
        tr("Settings imported. Devices are matched by their names; check the headphone profile and the microphone.")
          .to_string(),
      )
    } else {
      (Severity::Error, trf("Import failed: {}", &[&result]))
    });
  }
}

pub fn render_tab_settings(ui: &mut Ui, state: &mut AppState) {
  let icon_color = theme::palette(ui).text_secondary;
  theme::page_header(ui, icons::SETTINGS, icon_color, tr("Settings"), None);

  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::COLOR, icon_color)), tr("App theme"));
    ui.label(theme::caption(tr("Light or dark. \"Windows setting\" follows Settings > Personalization > Colors.")));
    ui.horizontal(|ui| {
      let mut pick = state.prefs.theme;
      ui.selectable_value(&mut pick, ThemePref::System, tr("Windows setting"));
      ui.selectable_value(&mut pick, ThemePref::Light, tr("Light"));
      ui.selectable_value(&mut pick, ThemePref::Dark, tr("Dark"));
      if pick != state.prefs.theme {
        state.prefs.theme = pick;
        ui.ctx().set_theme(state.prefs.egui_theme());
        state.prefs.save();
      }
    });
  });

  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::GLOBE, icon_color)), tr("Language"));
    let system_name = match i18n::system_language() {
      i18n::Lang::Cs => "Čeština",
      i18n::Lang::En => "English",
    };
    let label_of = |pref: LanguagePref| match pref {
      LanguagePref::System => trf("Windows language ({})", &[&system_name]),
      other => LANGUAGES.iter().find(|(p, _)| *p == other).map_or(String::new(), |(_, n)| n.to_string()),
    };
    let mut pick = state.prefs.language;
    egui::ComboBox::from_id_salt("language_pref").selected_text(label_of(pick)).width(260.0).show_ui(ui, |ui| {
      ui.selectable_value(&mut pick, LanguagePref::System, label_of(LanguagePref::System));
      for (pref, _) in LANGUAGES {
        ui.selectable_value(&mut pick, pref, label_of(pref));
      }
    });
    if pick != state.prefs.language {
      state.prefs.language = pick;
      i18n::set_language(pick);
      state.prefs.save();
    }
    ui.label(theme::caption(tr("The tray menu switches language the next time the background service starts.")));
  });

  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::POWER, icon_color)), tr("Startup and background service"));
    ui.horizontal(|ui| {
      let mut on = state.autostart_enabled;
      if ui
        .add(theme::Toggle(&mut on))
        .on_hover_text(tr("Starts the background service when you sign in to Windows."))
        .changed()
      {
        state.autostart_enabled = on;
        if let Err(e) = crate::autostart::set_autostart(on) {
          log::warn!("[Settings] autostart change failed: {e:?}");
        }
      }
      ui.label(tr("Start with Windows"));
    });
    if state.is_daemon_active() {
      theme::status_line(
        ui,
        Severity::Success,
        tr("Audio keeps being processed in the background when you close this window."),
      );
    } else {
      theme::status_line(
        ui,
        Severity::Info,
        tr("Processing runs inside this window only and stops when you close it."),
      );
    }
    ui.horizontal(|ui| {
      let mut on = !state.prefs.tray_mic_level_off;
      if ui.add(theme::Toggle(&mut on)).changed() {
        state.prefs.tray_mic_level_off = !on;
        state.prefs.save();
      }
      ui.label(tr("Show the microphone level in the tray icon"));
    });
    ui.label(theme::caption(tr("While an app records, the bars in the icon follow what others hear from you; only with microphone processing on. They stand still behind a full-screen app and while Windows keeps the icon among the hidden icons: drag it to the taskbar.")));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
      if ui
        .button(theme::with_icon(icons::CLOSE, tr("Close window")))
        .on_hover_text(tr("Closes only this window; the background service keeps processing audio."))
        .clicked()
      {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
      }
      if ui
        .button(
          RichText::new(theme::with_icon(icons::POWER, tr("Stop service and exit"))).color(theme::palette(ui).err),
        )
        .on_hover_text(tr("Stops all processing (background service included) and closes the app."))
        .clicked()
      {
        state.signal_daemon_shutdown();
        // A normal close: `on_exit` still saves the window and pending edits.
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
      }
    });
  });

  transfer_card(ui, state);
  uninstall_card(ui, state);

  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::INFO, icon_color)), tr("About"));
    ui.label(concat!("GamersEqualizer ", env!("GAMERSEQUALIZER_VERSION")));
    ui.label(format!("{}: {}", tr("Author"), env!("CARGO_PKG_AUTHORS")));
  });
}

fn transfer_card(ui: &mut Ui, state: &mut AppState) {
  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::SAVE, theme::palette(ui).text_secondary)), tr("Back up or move settings"));
    ui.label(theme::caption(tr(
      "Saves profiles and settings into one file, e.g. before reinstalling Windows or for a new PC. Devices are found again by their names, also on another USB port. The audio effects are installed on each PC separately.",
    )));
    let base_dir = state.storage.base_dir().to_path_buf();
    let t = &mut state.transfer_ui;
    ui.horizontal(|ui| {
      if ui.button(theme::with_icon(icons::SAVE, tr("Export settings..."))).clicked() {
        t.export = Some(settings_transfer::parts_here(&base_dir).into_iter().map(|p| (p, true)).collect());
        t.pending = None;
        t.message = None;
      }
      if ui.button(theme::with_icon(icons::OPEN, tr("Import settings..."))).clicked()
        && let Some(path) = shell::pick_settings_file(tr("Import settings"))
      {
        match settings_transfer::inspect(&path) {
          Ok(s) => {
            let parts = s.parts.iter().map(|p| (p.clone(), true)).collect();
            t.pending = Some((s, parts));
            t.export = None;
            t.message = None;
          }
          Err(e) => t.message = Some((Severity::Error, trf("Import failed: {}", &[&e]))),
        }
      }
    });

    if let Some(parts) = &mut t.export {
      ui.add_space(4.0);
      ui.label(theme::strong(tr("What goes into the file:"), 14.0));
      part_checkboxes(ui, parts);
      ui.label(theme::caption(tr(
        "Sharing profiles with a friend? Keep only the profiles: their devices and microphone settings stay as they are.",
      )));
      let only = selection(parts);
      let mut close = false;
      ui.horizontal(|ui| {
        if ui.add_enabled(!only.is_empty(), egui::Button::new(theme::with_icon(icons::SAVE, tr("Save...")))).clicked()
          && let Some(path) =
            shell::pick_settings_save_path(tr("Export settings"), &settings_transfer::default_file_name())
        {
          t.message = Some(match settings_transfer::export(&base_dir, &path, Some(&only)) {
            Ok(s) => (Severity::Success, trf("Saved {} profiles to {}", &[&s.profiles, &path.display()])),
            Err(e) => (Severity::Error, trf("Export failed: {}", &[&e])),
          });
          close = true;
        }
        close |= ui.button(tr("Cancel")).clicked();
      });
      if close {
        t.export = None;
      }
    }

    let mut import = None;
    if let Some((s, parts)) = &mut t.pending {
      ui.add_space(4.0);
      ui.label(theme::strong(
        trf("{}: {} profiles, exported {}", &[&s.path.display(), &s.profiles, &s.exported_at]),
        14.0,
      ));
      if !s.devices.is_empty() {
        ui.label(theme::caption(trf("Devices in the file: {}", &[&s.devices.join(", ")])));
      }
      ui.label(theme::strong(tr("What to import:"), 14.0));
      part_checkboxes(ui, parts);
      theme::status_line(
        ui,
        Severity::Warning,
        tr(
          "Your current settings are backed up first. Checked settings are replaced, profiles with the same name too; everything else stays. The app closes and starts again.",
        ),
      );
      let only = selection(parts);
      ui.horizontal(|ui| {
        if ui
          .add_enabled(!only.is_empty(), egui::Button::new(theme::with_icon(icons::OPEN, tr("Import and restart"))))
          .clicked()
        {
          import = Some(Some((s.path.clone(), only)));
        }
        if ui.button(tr("Cancel")).clicked() {
          import = Some(None);
        }
      });
    }
    // Some(None): cancelled.
    let import = match import {
      Some(None) => {
        t.pending = None;
        None
      }
      other => other.flatten(),
    };
    if let Some((path, only)) = import {
      crate::import_after_close(state, path, only);
      ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
    }
    if let Some((severity, text)) = &state.transfer_ui.message {
      theme::status_line(ui, *severity, text);
    }
  });
}

fn part_checkboxes(ui: &mut Ui, parts: &mut [(Part, bool)]) {
  for (part, on) in parts {
    ui.checkbox(on, part_label(part));
  }
}

fn selection(parts: &[(Part, bool)]) -> Selection {
  parts.iter().filter(|(_, on)| *on).map(|(p, _)| p.key.clone()).collect()
}

fn part_label(part: &Part) -> String {
  let name = match part.key.as_str() {
    "profiles/game" => tr("Game profiles"),
    "profiles/headphones" => tr("Headphone profiles"),
    "profiles/mic" => tr("Microphone profiles"),
    "profiles/chat" => tr("Chat profiles"),
    "profiles/media" => tr("Media profiles"),
    "profiles/aux" => tr("Aux profiles"),
    "settings/output_eq" => tr("Headphone and game EQ settings (devices, game assignments)"),
    "settings/audio_settings" => tr("Microphone and volume settings"),
    "settings/mic_active" => tr("Current microphone EQ"),
    "settings/sidetone" => tr("Sidetone"),
    "settings/app_channels" => tr("Apps assigned to mixer channels"),
    "settings/ui_prefs" => tr("Language and theme"),
    other => other,
  };
  if part.key.starts_with("profiles/") { format!("{name} ({})", part.profiles) } else { name.to_string() }
}

fn uninstall_card(ui: &mut Ui, state: &mut AppState) {
  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::DELETE, theme::palette(ui).text_secondary)), tr("Uninstall from Windows"));
    ui.label(theme::caption(tr(
      "Removes everything the app put into Windows: the microphone and output effects on all audio devices (administrator rights, audio stops for a few seconds), their backups, the app itself in Program Files, start with Windows and the notifications. Then the app exits.",
    )));
    if !state.uninstall_ui.confirming {
      if ui.button(theme::with_icon(icons::DELETE, tr("Uninstall..."))).clicked() {
        state.uninstall_ui.confirming = true;
        state.uninstall_ui.error = None;
      }
    } else {
      ui.checkbox(&mut state.uninstall_ui.remove_user_data, tr("Also delete my settings, profiles and logs"));
      let mut go = false;
      ui.horizontal(|ui| {
        let err = theme::palette(ui).err;
        go = ui.button(RichText::new(theme::with_icon(icons::DELETE, tr("Uninstall and exit"))).color(err)).clicked();
        if ui.button(tr("Cancel")).clicked() {
          state.uninstall_ui.confirming = false;
        }
      });
      if go {
        match crate::uninstall::uninstall_all(state.uninstall_ui.remove_user_data) {
          Ok(()) => {
            // The data cleanup waits for this process to end, so saving on exit is harmless.
            state.signal_daemon_shutdown();
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
          }
          Err(e) => {
            log::warn!("[Settings] uninstall failed: {e}");
            state.uninstall_ui.error = Some(e);
            state.uninstall_ui.confirming = false;
          }
        }
      }
    }
    if let Some(e) = &state.uninstall_ui.error {
      theme::status_line(ui, Severity::Error, &trf("Uninstall failed: {}", &[e]));
    }
    ui.label(theme::caption(tr("The app folder itself is not deleted; remove it yourself after the app exits.")));
  });
}
