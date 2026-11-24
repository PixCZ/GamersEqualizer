//! Window frame in the style of Windows 11 Settings: a slim top bar (app, service status,
//! master switch), a navigation pane on the left, and the output device card and picker
//! shared by the Headphones and Mixer pages.

use super::state::{ActiveTab, AppState};
use super::theme::{self, Icon, icons};
use crate::i18n::{tk, tr, trf};
use crate::win_audio::dolby_atmos::{active_format, spatial_status};
use crate::win_audio::{SpatialAudioStatus, WinAudioSystem, shell};
use egui::{Color32, RichText, Ui};
use std::time::{Duration, Instant};

pub const NAV_WIDTH: f32 = 248.0;
/// One spatial-audio activation + a few registry reads.
const SPATIAL_RECHECK: Duration = Duration::from_secs(3);
const BRAND: Color32 = Color32::from_rgb(255, 120, 40);

/// One row of the navigation pane.
enum Nav {
  Section(&'static str),
  Item(ActiveTab, Icon, &'static str, &'static str),
}

/// Navigation order: (tab, icon, label, short explanation).
const NAV: [Nav; 12] = [
  Nav::Section(tk("Output")),
  Nav::Item(ActiveTab::Mixer, icons::VOLUME, tk("Mixer & device"), tk("Volumes per app group, output device, ChatMix")),
  Nav::Item(
    ActiveTab::Headphones,
    icons::HEADPHONES,
    tk("Output & headphones"),
    tk("Output device and the correction for your headphones (AutoEQ)"),
  ),
  Nav::Item(ActiveTab::Game, icons::GAME, tk("Game"), tk("Game profiles, automatic game detection")),
  Nav::Item(ActiveTab::Chat, icons::CHAT, tk("Chat"), tk("Voice EQ for Discord & co., instead of the game profile")),
  Nav::Item(ActiveTab::Media, icons::MEDIA, tk("Media"), tk("Music / video EQ for browsers and players")),
  Nav::Section(tk("Sidetone")),
  Nav::Item(ActiveTab::Sidetone, icons::HEADPHONES, tk("Sidetone"), tk("Your own voice in the headphones")),
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
  // The bar moves the window like a title bar: drag to move, double-click to maximize.
  // Registered before the widgets, so the switch and status pill keep their own clicks;
  // labels don't select text here, or the app name would swallow the drag.
  let bar = ui.interact(ui.max_rect(), ui.scope_id().with("window_drag"), egui::Sense::click_and_drag());
  ui.style_mut().interaction.selectable_labels = false;
  if bar.drag_started_by(egui::PointerButton::Primary) {
    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
  } else if bar.double_clicked() {
    let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
  }
  ui.horizontal(|ui| {
    ui.add_space(8.0);
    let brand = theme::adapt(ui, BRAND);
    theme::icon_label(ui, icons::EQUALIZER, 18.0, brand, theme::strong("GamersEqualizer", 15.0));
    ui.add_space(8.0);
    let p = theme::palette(ui);
    // Only what does not run is said: "running" next to "Sound adjustments off" read as a
    // contradiction, and "no background service" alone hid that the effect may be down too.
    let service_down = !state.is_daemon_active();
    let effect_down = super::apo_banner::effect_not_running(state);
    if service_down || effect_down {
      let parts: Vec<&str> = [service_down.then(|| tr("service")), effect_down.then(|| tr("Windows effect"))]
        .into_iter()
        .flatten()
        .collect();
      let hover = if service_down {
        tr("Processing runs inside this window only and stops when you close it.")
      } else {
        tr("Apps get the sound unprocessed. The message below says what to do.")
      };
      status_pill(ui, p.warn, &trf("Not running: {}", &[&parts.join(", ")])).on_hover_text(hover);
    }

    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      ui.add_space(8.0);
      master_switch(ui, state);
    });
  });
}

/// Left navigation pane with the window/service actions at the bottom.
pub fn render_nav(ui: &mut Ui, state: &mut AppState) {
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
  });
}

/// Output device, hijack lock and spatial sound status (Headphones page).
pub fn render_device_card(ui: &mut Ui, state: &mut AppState, win_audio: &mut WinAudioSystem) {
  theme::card(ui, |ui| {
    ui.horizontal_wrapped(|ui| {
      let c = theme::palette(ui).text;
      theme::icon_label(ui, icons::SPEAKERS, 16.0, c, theme::strong(tr("Output device"), 15.0));
      if let Some(id) = output_combo(ui, state, "output_device_combo", 340.0, 44) {
        select_output(state, win_audio, id);
      }

      let is_locked = win_audio.endpoint_lock.is_locked();
      let hijacks = win_audio.endpoint_lock.hijack_count();
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
    render_spatial_row(ui, state);
  });
}

/// Re-reads spatial sound on the default output every `SPATIAL_RECHECK` (or right after
/// `spatial_checked_at = None`). Call once per frame.
pub fn refresh_spatial(state: &mut AppState, win_audio: &mut WinAudioSystem) {
  if state.spatial_checked_at.is_some_and(|t| t.elapsed() < SPATIAL_RECHECK) {
    return;
  }
  state.spatial_checked_at = Some(Instant::now());
  // Changes are logged by the service (`health`), which checks whether or not the window is open.
  let output = win_audio.endpoint_lock.get_default_output_id();
  state.spatial_status = output.as_deref().map_or(SpatialAudioStatus::Unsupported, spatial_status);
  let active = state.spatial_status.is_active();
  state.spatial_format = output.as_deref().filter(|_| active).and_then(active_format);
  // Learned by the background service (`health`); the window only reads it.
  state.spatial_expected =
    output.as_deref().and_then(|id| crate::health::expected_spatial(state.storage.base_dir(), id));
  state.spatial_output_id = output;
}

/// Output device picker (the Windows default output); returns the device picked this frame,
/// to pass to `select_output`. Shared by the device card and the mixer's Master strip, so
/// both change the same thing.
pub fn output_combo(ui: &mut Ui, state: &AppState, id: &str, width: f32, max_chars: usize) -> Option<String> {
  let current_name = state
    .endpoints
    .iter()
    .find(|e| Some(&e.id) == state.selected_endpoint_id.as_ref())
    .map(|e| e.name.clone())
    .unwrap_or_else(|| tr("Default System Output").to_string());
  let mut picked = None;
  egui::ComboBox::from_id_salt(id)
    .selected_text(super::truncate_chars(&current_name, max_chars))
    .width(width)
    .show_ui(ui, |ui| {
      for endpoint in &state.endpoints {
        let selected = Some(&endpoint.id) == state.selected_endpoint_id.as_ref();
        if ui.selectable_label(selected, &endpoint.name).clicked() {
          picked = Some(endpoint.id.clone());
        }
      }
    })
    .response
    .on_hover_text(&current_name);
  picked
}

/// Makes `id` the output: Windows default, Master, lock target and spatial sound check.
pub fn select_output(state: &mut AppState, win_audio: &mut WinAudioSystem, id: String) {
  win_audio.select_output(state.storage.base_dir(), &id);
  state.selected_endpoint_id = Some(id);
  state.spatial_checked_at = None;
  state.headphones_ui.recheck_apo();
  super::setup::recheck(state);
}

// --- private helpers ---------------------------------------------------------------------

/// "Spatial sound: Dolby Atmos for Headphones" + a button to the Windows page where it is
/// chosen. Warning color, icon and "Keep" when it is not the format the user had.
fn render_spatial_row(ui: &mut Ui, state: &mut AppState) {
  let active = state.spatial_status.is_active();
  let current_name = match (&state.spatial_format, active) {
    (Some(f), true) => f.label(tr("Active")).to_string(),
    _ => tr(state.spatial_status.label()).to_string(),
  };
  // Off (with a known format before) or another format: the user's setting was lost.
  let lost = match (&state.spatial_expected, &state.spatial_format) {
    (Some(expected), Some(now)) => (expected.id != now.id).then_some(expected),
    (Some(expected), None) if state.spatial_status == SpatialAudioStatus::Disabled => Some(expected),
    _ => None,
  };
  let p = theme::palette(ui);
  let mut keep = None;
  ui.horizontal_wrapped(|ui| {
    if let Some(expected) = lost {
      let text = trf(
        "Spatial sound: {} instead of {}",
        &[&current_name, &expected.label(tr("your format"))],
      );
      ui.label(RichText::new(theme::with_icon(icons::WARNING, &text)).small().color(p.warn))
        .on_hover_text(tr("Something changed spatial sound on this device (a Windows or driver update, another app). Switch it back in the Windows settings, or keep the new one."));
      if let Some(now) = state.spatial_format.clone()
        && ui.button(trf("Keep \"{}\"", &[&now.label(tr("the new format"))])).clicked() {
          keep = Some(now);
        }
    } else {
      let color = if active { theme::adapt(ui, Color32::from_rgb(0, 180, 216)) } else { p.text_tertiary };
      status_pill(ui, color, &format!("{}: {}", tr("Spatial sound"), current_name))
        .on_hover_text(tr("Windows spatial sound on this device (Dolby Atmos, DTS:X, Windows Sonic). The headphone and game EQ run after it."));
    }
    if ui
      .button(tr("Change…"))
      .on_hover_text(tr("Opens the Windows settings of this device, where spatial sound is chosen."))
      .clicked()
    {
      shell::open_spatial_sound_settings();
    }
  });
  if let (Some(format), Some(id)) = (keep, state.spatial_output_id.clone()) {
    crate::health::remember_spatial(state.storage.base_dir(), &id, &format);
    state.spatial_checked_at = None;
  }
}

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
  // The switch is what the user wants; whether Windows really runs it is the service's verdict.
  let broken: Vec<String> = super::apo_banner::run_status(state)
    .map(|r| [r.mic_not_running.as_ref(), r.output_not_running.as_ref()].into_iter().flatten().cloned().collect())
    .unwrap_or_default();
  let p = theme::palette(ui);
  let (title, detail, color) = if on && !broken.is_empty() {
    (tr("Sound adjustments on, but they do not work"), trf("Not working on: {}", &[&broken.join(", ")]), p.err)
  } else if on {
    (tr("Sound adjustments on"), tr("Headphone & game EQ, mic filters").to_string(), p.text)
  } else {
    (tr("Sound adjustments off"), tr("You hear the original sound, as without the app").to_string(), p.warn)
  };
  let clicked = ui.add(theme::Toggle(&mut v)).on_hover_text(hint).clicked();
  ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
    ui.spacing_mut().item_spacing.y = 0.0;
    ui.label(theme::strong(title, 14.0).color(color)).on_hover_text(hint);
    ui.label(theme::caption(detail)).on_hover_text(hint);
  });
  if clicked {
    state.output_eq.settings.enabled = !on;
    state.output_eq_do(|eq, shm| eq.save_settings(shm));
  }
}
