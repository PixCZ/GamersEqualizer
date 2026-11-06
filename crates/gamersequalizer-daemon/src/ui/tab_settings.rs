//! Settings page: appearance (light/dark), language, start with Windows, and the window /
//! service actions that used to sit at the bottom of the navigation pane.

use super::prefs::ThemePref;
use super::state::AppState;
use super::theme::{self, icons, Severity};
use crate::i18n::{self, tr, trf, LanguagePref, LANGUAGES};
use egui::{Color32, RichText, Ui};

const COLOR: Color32 = Color32::from_rgb(150, 150, 150);

pub fn render_tab_settings(ui: &mut Ui, state: &mut AppState) {
  theme::page_header(ui, icons::SETTINGS, COLOR, tr("Settings"), None);

  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::COLOR, COLOR)), tr("App theme"));
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
    theme::card_title(ui, Some((icons::GLOBE, COLOR)), tr("Language"));
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
    theme::card_title(ui, Some((icons::POWER, COLOR)), tr("Startup and background service"));
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
        std::process::exit(0);
      }
    });
  });

  theme::card(ui, |ui| {
    theme::card_title(ui, Some((icons::INFO, COLOR)), tr("About"));
    ui.label(format!("GamersEqualizer {}", env!("CARGO_PKG_VERSION")));
  });
}
