//! Window frame in the style of Windows 11 Settings: a slim top bar (app, service status,
//! quick headphone switch, master switch), a navigation pane on the left, and the output
//! device card shown on the Mixer page.

use super::state::{ActiveTab, AppState};
use super::theme::{self, icons, Icon};
use crate::i18n::{tk, tr, trf};
use crate::win_audio::WinAudioSystem;
use egui::{Color32, RichText, Ui};
use gamersequalizer_common::profile::AudioMode;

pub const NAV_WIDTH: f32 = 248.0;
const BRAND: Color32 = Color32::from_rgb(255, 120, 40);

/// One row of the navigation pane.
enum Nav {
  Section(&'static str),
  Item(ActiveTab, Icon, &'static str, &'static str),
}

/// Navigation order: (tab, icon, label, short explanation).
const NAV: [Nav; 10] = [
  Nav::Section(tk("Output")),
  Nav::Item(ActiveTab::Mixer, icons::VOLUME, tk("Mixer & device"), tk("Volumes per app group, output device, ChatMix")),
  Nav::Item(ActiveTab::Headphones, icons::HEADPHONES, tk("Headphones"), tk("Correction for your headphones (AutoEQ)")),
  Nav::Item(ActiveTab::Game, icons::GAME, tk("Game"), tk("Game profiles, automatic game detection")),
  Nav::Item(ActiveTab::Chat, icons::CHAT, tk("Chat"), tk("Voice EQ for Discord & co., instead of the game profile")),
  Nav::Item(ActiveTab::Media, icons::MEDIA, tk("Media"), tk("Music / video EQ for browsers and players")),
  Nav::Section(tk("Input")),
  Nav::Item(ActiveTab::Mic, icons::MIC, tk("Microphone"), tk("Echo cancellation, noise suppression, mic EQ")),
  Nav::Section(tk("Diagnostics")),
  Nav::Item(
    ActiveTab::Log,
    icons::LOG,
    tk("Log"),
    tk("What was detected and done: games, profiles, mic, CPU overload"),
  ),
];

/// Top bar: always visible, only what is needed from every page.
pub fn render_top_bar(ui: &mut Ui, state: &mut AppState) {
  ui.horizontal(|ui| {
    ui.add_space(8.0);
    let brand = theme::adapt(ui, BRAND);
    theme::icon_label(ui, icons::EQUALIZER, 18.0, brand, theme::strong("GamersEqualizer", 15.0));
    ui.add_space(8.0);
    let p = theme::palette(ui);
    if state.is_daemon_active() {
      status_pill(ui, p.ok, tr("Service running"))
        .on_hover_text(tr("Audio keeps being processed in the background when you close this window."));
    } else {
      status_pill(ui, p.text_tertiary, tr("No background service"))
        .on_hover_text(tr("Processing runs inside this window only and stops when you close it."));
    }

    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      ui.add_space(8.0);
      master_switch(ui, state);
      ui.add_space(8.0);
      headphone_switch(ui, state);
    });
  });
}

/// Left navigation pane with the window/service actions at the bottom.
pub fn render_nav(ui: &mut Ui, state: &mut AppState, win_audio: &WinAudioSystem) {
  ui.add_space(4.0);
  ui.spacing_mut().item_spacing.y = 2.0;
  for row in &NAV {
    match row {
      Nav::Section(label) => theme::nav_section(ui, tr(label)),
      Nav::Item(tab, ic, label, hint) => {
        if theme::nav_item(ui, state.active_tab == *tab, *ic, tr(label)).on_hover_text(tr(hint)).clicked() {
          state.active_tab = *tab;
        }
      }
    }
  }

  ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
    ui.add_space(8.0);
    if theme::nav_item(ui, state.active_tab == ActiveTab::Settings, icons::SETTINGS, tr("Settings"))
      .on_hover_text(tr("Language, theme, start with Windows, stop the service"))
      .clicked()
    {
      state.active_tab = ActiveTab::Settings;
    }
    ui.add_space(6.0);
    let output = state
      .endpoints
      .iter()
      .find(|e| Some(&e.id) == win_audio.chatmix.master_device_id.as_ref())
      .map(|e| e.name.clone())
      .unwrap_or_else(|| tr("Default output").to_string());
    ui.horizontal(|ui| {
      ui.add_space(12.0);
      let c = theme::palette(ui).text_tertiary;
      theme::icon_label(ui, icons::VOLUME, 12.0, c, theme::caption(super::truncate_chars(&output, 30)));
    })
    .response
    .on_hover_text(trf("Current output: {}\nChange it on the Mixer & device page.", &[&output]));
  });
}

/// Output device, hijack lock, headphones/speakers mode and spatial sound status (Mixer page).
pub fn render_device_card(ui: &mut Ui, state: &mut AppState, win_audio: &mut WinAudioSystem) {
  theme::card(ui, |ui| {
    ui.horizontal_wrapped(|ui| {
      let c = theme::palette(ui).text;
      theme::icon_label(ui, icons::SPEAKERS, 16.0, c, theme::strong(tr("Output device"), 14.0));

      let is_locked = win_audio.endpoint_lock.is_locked();
      let hijacks = win_audio.endpoint_lock.hijack_count();
      let current_name = state
        .endpoints
        .iter()
        .find(|e| Some(&e.id) == state.selected_endpoint_id.as_ref())
        .map(|e| e.name.clone())
        .unwrap_or_else(|| tr("Default System Output").to_string());

      egui::ComboBox::from_id_salt("output_device_combo")
        .selected_text(super::truncate_chars(&current_name, 44))
        .width(340.0)
        .show_ui(ui, |ui| {
          for endpoint in &state.endpoints {
            let selected = Some(&endpoint.id) == state.selected_endpoint_id.as_ref();
            if ui.selectable_label(selected, &endpoint.name).clicked() {
              state.selected_endpoint_id = Some(endpoint.id.clone());
              win_audio.chatmix.master_device_id = Some(endpoint.id.clone());
              let _ = win_audio.endpoint_lock.set_as_default_device(&endpoint.id);
              // Picked here = intended: the tray must not report it as a lost default.
              crate::health::remember_output(state.storage.base_dir(), &endpoint.id);
              if is_locked {
                win_audio.endpoint_lock.lock_device(endpoint.id.clone());
              }
              state.spatial_status = win_audio.dolby_guard.check_spatial_status();
              win_audio.chatmix.sync_from_system();
            }
          }
        });

      let p = theme::palette(ui);
      let lock_text = if is_locked {
        RichText::new(theme::with_icon(icons::LOCK, &trf("Locked ({} blocked)", &[&hijacks]))).color(p.ok)
      } else {
        RichText::new(theme::with_icon(icons::UNLOCK, tr("Lock device")))
      };
      if ui
        .button(lock_text)
        .on_hover_text(tr("Keeps Windows on this device. If a driver update or an HDMI monitor takes over the default output, it is switched back."))
        .clicked()
      {
        if is_locked {
          win_audio.endpoint_lock.unlock();
        } else if let Some(ref id) = state.selected_endpoint_id {
          win_audio.endpoint_lock.lock_device(id.clone());
        } else if let Some(id) = win_audio.endpoint_lock.get_default_output_id() {
          state.selected_endpoint_id = Some(id.clone());
          win_audio.endpoint_lock.lock_device(id);
        }
      }
    });

    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
      ui.label(RichText::new(tr("Mode")).color(theme::palette(ui).text_secondary));
      let is_hp = state.audio_mode == AudioMode::Headphones;
      if ui.selectable_label(is_hp, theme::with_icon(icons::HEADPHONES, tr("Headphones"))).clicked() {
        state.audio_mode = AudioMode::Headphones;
      }
      if ui.selectable_label(!is_hp, theme::with_icon(icons::SPEAKERS, tr("Speakers"))).clicked() {
        state.audio_mode = AudioMode::Speakers;
      }
      ui.add_space(12.0);
      let active = state.spatial_status.is_active();
      let p = theme::palette(ui);
      let color = if active { theme::adapt(ui, Color32::from_rgb(0, 180, 216)) } else { p.text_tertiary };
      status_pill(ui, color, &format!("{}: {}", tr("Spatial sound"), tr(state.spatial_status.label())))
        .on_hover_text(tr("Windows spatial sound on this device (Dolby Atmos, DTS:X, Windows Sonic). The headphone and game EQ run after it."));
    });
  });
}

// --- private helpers ---------------------------------------------------------------------

/// Colored dot + secondary text, the Windows way to show a state next to a title.
fn status_pill(ui: &mut Ui, color: Color32, text: &str) -> egui::Response {
  ui.horizontal(|ui| {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
    ui.label(RichText::new(text).small().color(theme::palette(ui).text_secondary));
  })
  .response
}

fn master_switch(ui: &mut Ui, state: &mut AppState) {
  let on = state.output_eq.settings.enabled;
  let hint = tr(
    "One switch for everything: headphone correction, game EQ and the microphone chain (echo cancellation, \
     noise suppression, EQ, boost). OFF = what you would hear without this app.",
  );
  let mut v = on;
  let label = if on { tr("Processing on") } else { tr("All off (original sound)") };
  let p = theme::palette(ui);
  let clicked = ui.add(theme::Toggle(&mut v)).on_hover_text(hint).clicked();
  ui.label(theme::strong(label, 14.0).color(if on { p.text } else { p.warn })).on_hover_text(hint);
  if clicked {
    state.output_eq.settings.enabled = !on;
    state.output_eq_do(|eq, shm| eq.save_settings(shm));
  }
}

/// Quick headphone switch (remembered for the current output device).
fn headphone_switch(ui: &mut Ui, state: &mut AppState) {
  let current = state.output_eq.headphone_profile_name().map(str::to_string);
  let mut picked: Option<Option<String>> = None;
  let mut add = false;
  egui::ComboBox::from_id_salt("top_headphones")
    .selected_text(theme::with_icon(
      icons::HEADPHONES,
      &super::truncate_chars(current.as_deref().unwrap_or(tr("No correction")), 26),
    ))
    .width(240.0)
    .show_ui(ui, |ui| {
      if ui.selectable_label(current.is_none(), tr("No correction (flat)")).clicked() {
        picked = Some(None);
      }
      for p in &state.output_eq.headphone_profiles {
        if ui.selectable_label(current.as_deref() == Some(p.name.as_str()), &p.name).clicked() {
          picked = Some(Some(p.name.clone()));
        }
      }
      add = ui.selectable_label(false, format!("➕  {}", tr("Add headphones..."))).clicked();
    })
    .response
    .on_hover_text(tr(
      "Switch between your headphones. Remembered per output device: changing the output switches it too.",
    ));
  if let Some(choice) = picked {
    state.output_eq_do(|eq, shm| eq.select_headphone(choice, shm));
  }
  if add {
    state.active_tab = ActiveTab::Headphones;
    state.headphones_ui.open_wizard();
  }
}
