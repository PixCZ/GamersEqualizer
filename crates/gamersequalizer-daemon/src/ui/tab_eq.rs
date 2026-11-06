//! Game tab (game profile with auto-detection, on top of the headphone correction) and the
//! Chat / Media tabs.

use super::eq_editor::{band_editor, curves_plot, on_off_button, profile_picker};
use super::state::{ActiveTab, AppState, OutputLayer};
use super::theme::{self, icons, Severity};
use crate::dsp_apo::generate_log_frequencies;
use crate::i18n::{tk, tr, trf};
use crate::output_eq::game_match::MatchMethod;
use crate::sonar_importer::{import_sonar_db, DEFAULT_SONAR_DB_PATH};
use egui::{Color32, RichText, Ui};
use gamersequalizer_common::biquad::MAX_OUT_EQ_BANDS;
use gamersequalizer_common::streams::StreamBus;
use std::path::Path;

const GAME_COLOR: Color32 = Color32::from_rgb(82, 183, 136);
const CHAT_COLOR: Color32 = Color32::from_rgb(72, 202, 228);
const MEDIA_COLOR: Color32 = Color32::from_rgb(247, 37, 133);
const HP_COLOR: Color32 = Color32::from_rgb(100, 150, 220);
const LAYER_COLOR: Color32 = Color32::from_rgb(255, 180, 50);
const COMBINED_COLOR: Color32 = Color32::from_rgb(50, 230, 120);
/// Picker value meaning "decide automatically" in the per-app binding combo.
const AUTO: &str = tk("Automatic");
const NOT_A_GAME: &str = tk("Not a game");

pub fn render_tab_eq(ui: &mut Ui, state: &mut AppState, tab: ActiveTab) {
  match tab {
    ActiveTab::Chat | ActiveTab::Media => render_bus_tab(ui, state, tab),
    _ => render_game_tab(ui, state),
  }
}

fn render_game_tab(ui: &mut Ui, state: &mut AppState) {
  ui.horizontal(|ui| {
    theme::page_header(ui, icons::GAME, GAME_COLOR, tr("Game EQ"), None);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      if ui.button(format!("📥  {}", tr("Import from Sonar DB"))).on_hover_text(DEFAULT_SONAR_DB_PATH).clicked() {
        reimport_sonar(state);
      }
    });
  });
  ui.label(
    RichText::new(tr("Game profiles on top of the headphone correction, switched by the game in the foreground."))
      .color(theme::palette(ui).text_secondary),
  );
  ui.add_space(theme::CARD_GAP);

  // Switches and detection.
  theme::card(ui, |ui| {
    ui.horizontal_wrapped(|ui| {
      let s = state.output_eq.settings.clone();
      let mut next = s.clone();
      if on_off_button(ui, s.game_enabled, tr("Game EQ")) {
        next.game_enabled = !s.game_enabled;
      }
      ui.add_space(16.0);
      ui.checkbox(&mut next.auto_detect, tr("Auto-detect game"))
        .on_hover_text(tr("Switch the profile when a game comes to the foreground; it stays until the game exits."));
      ui.add_enabled_ui(next.auto_detect, |ui| {
        ui.checkbox(&mut next.detect_launchers, tr("Launchers"))
          .on_hover_text(tr("Steam, Epic, GOG, Ubisoft Connect and Microsoft Store data on this PC (e.g. division2.exe = Steam app 2221490 = The Division 2)"));
        ui.checkbox(&mut next.detect_names, tr("Names"))
          .on_hover_text(tr("Match the exe name / product name with profile names (e.g. \"Tom Clancy's The Division 2\")"));
      });
      if next != s {
        state.output_eq.settings = next;
        state.output_eq_do(|eq, shm| eq.save_settings(shm));
      }
    });
    ui.add_space(4.0);
    active_game_row(ui, state);
  });

  // Graph: the game profile (editable dots) over the headphone correction and the sum.
  theme::card(ui, |ui| {
    if state.output_eq.game_profile().is_some() {
      volume_and_delete_row(ui, state, OutputLayer::Game);
      ui.add_space(4.0);
    }
    let r = &state.output_eq.result;
    let (hp_curve, combined) = (r.layer_a_curve.gains_db.clone(), r.combined_curve.gains_db.clone());
    let height = super::eq_editor::plot_height(ui, 0.32, 170.0, 420.0);
    let mut changed = false;
    if let Some(profile) = state.output_eq.game_profile_mut() {
      let background = [
        (tr("Headphone correction"), hp_curve.as_slice(), HP_COLOR, 1.2),
        (tr("Combined"), combined.as_slice(), COMBINED_COLOR, 1.4),
      ];
      changed = band_editor(ui, "game_eq", &mut profile.bands, MAX_OUT_EQ_BANDS / 2, LAYER_COLOR, &background, height);
    } else {
      let curves = [
        (tr("Headphone correction"), hp_curve.as_slice(), HP_COLOR, 1.2),
        (tr("Combined (what you hear)"), combined.as_slice(), COMBINED_COLOR, 2.2),
      ];
      curves_plot(ui, "game_eq_plot", &generate_log_frequencies(), &curves, Some(state.output_eq.preamp_db), height);
      ui.label(theme::caption(tr("No game profile active.")));
    }
    legend(
      ui,
      &[
        (tr("game profile (dots)"), LAYER_COLOR),
        (tr("headphone correction"), HP_COLOR),
        (tr("combined, what you hear"), COMBINED_COLOR),
      ],
    );
    if state.output_eq.preamp_db != 0.0 {
      ui.label(theme::caption(trf("Auto preamp {} dB (no clipping)", &[&format!("{:.1}", state.output_eq.preamp_db)])));
    }
    if changed {
      state.apply_output_eq();
      state.out_eq_unsaved = Some(OutputLayer::Game);
    }
  });

  theme::card(ui, |ui| {
    egui::CollapsingHeader::new(theme::strong(tr("Recent apps: assign a profile manually"), 15.0))
      .show(ui, |ui| recent_apps(ui, state));
  });
}

/// Colored line samples with their meaning, under a graph.
fn legend(ui: &mut Ui, items: &[(&str, Color32)]) {
  ui.horizontal_wrapped(|ui| {
    for &(text, color) in items {
      let c = theme::adapt(ui, color);
      let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 12.0), egui::Sense::hover());
      ui.painter().line_segment([rect.left_center(), rect.right_center()], egui::Stroke::new(2.5_f32, c));
      ui.label(RichText::new(text).small().color(theme::palette(ui).text_secondary));
      ui.add_space(8.0);
    }
  });
}

/// "Detected: X -> profile" + the profile used when no game runs.
fn active_game_row(ui: &mut Ui, state: &mut AppState) {
  let names: Vec<String> = state.output_eq.game_profiles.iter().map(|p| p.name.clone()).collect();
  let refs: Vec<&str> = names.iter().map(String::as_str).collect();
  let mut filter = std::mem::take(&mut state.headphones_ui.filter);

  ui.horizontal_wrapped(|ui| {
    match state.output_eq.active_game.clone() {
      Some(game) => {
        let c = theme::adapt(ui, GAME_COLOR);
        theme::icon_label(ui, icons::GAME, 16.0, c, theme::strong(trf("Detected: {}", &[&game.label]), 14.0));
        // The exe of the active game, to bind it.
        let exe = state.output_eq.recent_apps.iter().find(|a| a.label == game.label).map(|a| a.exe_name.clone());
        ui.label(tr("→ profile:"));
        let current = (!game.profile.is_empty()).then_some(game.profile.as_str());
        if let Some(exe) = exe {
          if let Some(choice) =
            profile_picker(ui, "active_game_profile", current, &refs, tr("(none)"), &mut filter, 240.0)
          {
            state.output_eq_do(|eq, shm| eq.bind_exe(&exe, Some(choice.unwrap_or_default()), shm));
          }
        } else {
          ui.label(RichText::new(current.unwrap_or(tr("none"))).strong());
        }
        if ui
          .button(tr("Reset"))
          .on_hover_text(tr("Forget the detected game until it comes to the foreground again"))
          .clicked()
        {
          state.output_eq_do(|eq, shm| eq.clear_active_game(shm));
        }
      }
      None => {
        theme::status_line(ui, Severity::Info, tr("No game detected."));
      }
    }
  });
  ui.horizontal(|ui| {
    ui.label(tr("Profile when no game runs:"));
    let fallback = state.output_eq.settings.fallback_game_profile.clone();
    if let Some(choice) = profile_picker(
      ui,
      "fallback_game_profile",
      fallback.as_deref(),
      &refs,
      tr("None (no game EQ)"),
      &mut filter,
      240.0,
    ) {
      state.output_eq.settings.fallback_game_profile = choice;
      state.output_eq_do(|eq, shm| eq.save_settings(shm));
    }
  });
  state.headphones_ui.filter = filter;
}

fn recent_apps(ui: &mut Ui, state: &mut AppState) {
  ui.label(theme::caption(tr(
    "Apps that were in the foreground. Assign a profile if a game is not recognized, or mark an app as not a game.",
  )));
  let (auto, not_a_game) = (tr(AUTO), tr(NOT_A_GAME));
  let mut names: Vec<String> = vec![auto.to_string(), not_a_game.to_string()];
  names.extend(state.output_eq.game_profiles.iter().map(|p| p.name.clone()));
  let refs: Vec<&str> = names.iter().map(String::as_str).collect();
  let apps = state.output_eq.recent_apps.clone();
  if apps.is_empty() {
    ui.label(theme::caption(tr("Switch to a game window once, then come back here.")));
  }
  let mut filter = std::mem::take(&mut state.headphones_ui.filter);
  egui::Grid::new("recent_apps").num_columns(3).spacing([16.0, 8.0]).show(ui, |ui| {
    for (i, app) in apps.iter().enumerate() {
      ui.label(RichText::new(&app.label).strong());
      let how = match app.matched.as_ref().map(|m| (m.method, m.profile.as_deref())) {
        Some((MatchMethod::Binding, Some(p))) => trf("assigned: {}", &[&p]),
        Some((MatchMethod::LauncherId, Some(p))) => trf("launcher id: {}", &[&p]),
        Some((MatchMethod::Name, Some(p))) => trf("by name: {}", &[&p]),
        Some((_, _)) => tr("game, no profile").to_string(),
        None => tr("not a game").to_string(),
      };
      ui.label(theme::caption(how));
      let bound = state.output_eq.settings.bindings.get(&app.exe_name).cloned();
      let current = match bound.as_deref() {
        None => auto,
        Some("") => not_a_game,
        Some(p) => p,
      };
      if let Some(Some(choice)) =
        profile_picker(ui, &format!("bind_{i}"), Some(current), &refs, auto, &mut filter, 220.0)
      {
        let binding = match choice.as_str() {
          c if c == auto => None,
          c if c == not_a_game => Some(String::new()),
          p => Some(p.to_string()),
        };
        let exe = app.exe_name.clone();
        state.output_eq_do(|eq, shm| eq.bind_exe(&exe, binding, shm));
      }
      ui.end_row();
    }
  });
  state.headphones_ui.filter = filter;
}

/// Volume of the shown profile, the loudness-matching switch, the resulting volume change,
/// and "Delete" with a confirmation. Used by the Headphones, Game, Chat and Media tabs.
pub fn volume_and_delete_row(ui: &mut Ui, state: &mut AppState, layer: OutputLayer) {
  let name = match layer {
    OutputLayer::Headphone => state.output_eq.headphone_profile_name(),
    OutputLayer::Game => state.output_eq.game_profile_name(),
    OutputLayer::Chat => state.output_eq.bus_profile_name(StreamBus::Chat),
    OutputLayer::Media => state.output_eq.bus_profile_name(StreamBus::Media),
  }
  .map(str::to_string)
  .unwrap_or_default();

  ui.horizontal_wrapped(|ui| {
    ui.label(tr("Volume of this profile"));
    let profile = match layer {
      OutputLayer::Headphone => state.output_eq.headphone_profile_mut(),
      OutputLayer::Game => state.output_eq.game_profile_mut(),
      OutputLayer::Chat => state.output_eq.bus_profile_mut(StreamBus::Chat),
      OutputLayer::Media => state.output_eq.bus_profile_mut(StreamBus::Media),
    };
    let mut changed = false;
    if let Some(p) = profile {
      changed = ui
        .add(egui::Slider::new(&mut p.gain_db, -12.0..=12.0).suffix(" dB").max_decimals(1))
        .on_hover_text(tr("Make this profile louder or quieter by ear, e.g. so all your headphones sound equally loud. Saved with the profile."))
        .changed();
    }
    if changed {
      state.apply_output_eq();
      state.out_eq_unsaved = Some(layer);
    }
    ui.add_space(12.0);

    let mut matching = state.output_eq.settings.loudness_match;
    if ui
      .checkbox(&mut matching, tr("Match loudness between profiles"))
      .on_hover_text(tr(
        "Every EQ boost needs headroom, so the app turns the volume down first (preamp).\n\
         ON: by the average level between 200 Hz and 6 kHz, so switching profiles keeps the loudness. Rare peaks \
         above full scale (at most +3 dB) are turned down smoothly by a limiter in the output effect.\n\
         OFF: by the strongest boost. Never needs the limiter, but profiles with big boosts sound quieter.",
      ))
      .changed()
    {
      state.output_eq.settings.loudness_match = matching;
      state.output_eq_do(|eq, shm| eq.save_settings(shm));
    }
  });

  let (preamp, peak) = match layer.bus() {
    Some(bus) => {
      let (_, preamp, peak) = state.output_eq.bus_result(bus);
      (preamp, peak)
    }
    None => (state.output_eq.preamp_db, state.output_eq.peak_after_db),
  };
  let mut summary = trf("Total volume change: {} dB.", &[&format!("{preamp:+.1}")]);
  if peak > 0.05 {
    summary.push(' ');
    summary.push_str(&trf(
      "Strongest EQ peaks reach {} dB above full scale; the limiter turns them down.",
      &[&format!("{peak:+.1}")],
    ));
  }
  ui.label(theme::caption(summary));

  // Delete with confirmation.
  let pending = state.headphones_ui.confirm_delete.clone().filter(|(l, _)| *l == layer);
  match pending {
    None => {
      if ui.button(format!("🗑  {}", tr("Delete this profile"))).clicked() {
        state.headphones_ui.confirm_delete = Some((layer, name.clone()));
        state.headphones_ui.notice = None;
      }
    }
    Some((_, target)) => {
      theme::info_bar(ui, Severity::Warning, |ui| {
        let mut text =
          trf("Delete \"{}\"? The file moves to profiles\\_deleted and can be restored from there.", &[&target]);
        if layer != OutputLayer::Headphone {
          text.push(' ');
          text.push_str(tr("\"Import from Sonar DB\" brings Sonar presets back."));
        }
        ui.label(text);
        ui.horizontal(|ui| {
          if ui.add(theme::accent_button(ui, tr("Yes, delete"))).clicked() {
            let result = state.output_eq_do(|eq, shm| match layer {
              OutputLayer::Headphone => eq.delete_headphone_profile(&target, shm),
              OutputLayer::Game => eq.delete_game_profile(&target, shm),
              OutputLayer::Chat => eq.delete_bus_profile(StreamBus::Chat, &target, shm),
              OutputLayer::Media => eq.delete_bus_profile(StreamBus::Media, &target, shm),
            });
            state.headphones_ui.notice = Some(match result {
              Ok(path) => trf("Deleted \"{}\" (moved to {}).", &[&target, &path.display()]),
              Err(e) => trf("Could not delete \"{}\": {}", &[&target, &e]),
            });
            state.headphones_ui.confirm_delete = None;
          }
          if ui.button(tr("Cancel")).clicked() {
            state.headphones_ui.confirm_delete = None;
          }
        });
      });
    }
  }
  if let Some(notice) = state.headphones_ui.notice.as_deref() {
    ui.label(RichText::new(notice).small());
  }
}

fn reimport_sonar(state: &mut AppState) {
  let db_path = Path::new(DEFAULT_SONAR_DB_PATH);
  if !db_path.exists() {
    log::warn!("Sonar DB not found at {}", db_path.display());
    return;
  }
  match import_sonar_db(db_path, &state.storage) {
    Ok(stats) => {
      log::info!("Sonar import: {} profiles", stats.total_imported);
      state.available_mic_profiles = super::mic_presets::all_mic_presets(&state.storage);
      state.output_eq_do(|eq, shm| {
        eq.reload();
        eq.save_settings(shm);
      });
    }
    Err(e) => log::error!("Sonar DB import failed: {e}"),
  }
}

/// Chat / Media: the channel profile the stream EQ runs on those apps instead of the game
/// profile (headphone correction stays).
fn render_bus_tab(ui: &mut Ui, state: &mut AppState, tab: ActiveTab) {
  let (bus, layer, title, icon, color, apps) = match tab {
    ActiveTab::Chat => {
      (StreamBus::Chat, OutputLayer::Chat, tr("Chat EQ"), icons::CHAT, CHAT_COLOR, tr("Discord & co."))
    }
    _ => (
      StreamBus::Media,
      OutputLayer::Media,
      tr("Media EQ"),
      icons::MEDIA,
      MEDIA_COLOR,
      tr("browsers and music players"),
    ),
  };
  theme::page_header(
    ui,
    icon,
    color,
    title,
    Some(&trf("For {}: headphone correction + this profile, instead of the game profile.", &[&apps])),
  );

  theme::card(ui, |ui| {
    ui.horizontal_wrapped(|ui| {
      let on = state.output_eq.bus_enabled(bus);
      if on_off_button(ui, on, title) {
        match bus {
          StreamBus::Chat => state.output_eq.settings.chat_enabled = !on,
          _ => state.output_eq.settings.media_enabled = !on,
        }
        state.output_eq_do(|eq, shm| eq.save_settings(shm));
      }
      ui.add_space(16.0);
      let (severity, status) = match state.streams_on_bus(bus) {
        None => (Severity::Warning, tr("Stream EQ not running").to_string()),
        Some(0) => (Severity::Info, tr("No app stream of this channel recognized right now").to_string()),
        Some(n) => (Severity::Success, trf("Applied to {} app stream(s) now", &[&n])),
      };
      theme::status_line(ui, severity, &status).on_hover_text(tr(
        "Counts streams the background service has matched to an app of this channel. A stream is \
         recognized within about a second once it plays; silent streams aren't.",
      ));
    });
    ui.label(theme::caption(tr(
      "Applied by the stream EQ, an effect on each app's own stream before Windows mixes them (no virtual sound \
       card, no extra latency). Install it on the output once (admin): scripts\\apo-install.ps1 -Flow Render -Slot \
       SFX. Apps it can't match keep headphone + game EQ.",
    )));
    ui.add_space(4.0);

    // Profile picker; none = flat (headphone correction only).
    let names: Vec<String> = state.output_eq.bus_profiles(bus).iter().map(|p| p.name.clone()).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut filter = std::mem::take(&mut state.headphones_ui.filter);
    ui.horizontal(|ui| {
      ui.label(tr("Profile"));
      let current = state.output_eq.bus_profile_name(bus).map(str::to_string);
      if let Some(choice) = profile_picker(
        ui,
        "bus_profile",
        current.as_deref(),
        &refs,
        tr("Flat (headphone correction only)"),
        &mut filter,
        280.0,
      ) {
        state.output_eq_do(|eq, shm| eq.select_bus_profile(bus, choice, shm));
      }
    });
    state.headphones_ui.filter = filter;
  });

  theme::card(ui, |ui| {
    if state.output_eq.bus_profile(bus).is_some() {
      volume_and_delete_row(ui, state, layer);
      ui.add_space(4.0);
    }
    let (result, preamp, _) = state.output_eq.bus_result(bus);
    let mut changed = false;

    // Graph: this profile (editable dots) over the headphone correction and the sum.
    let (hp_curve, combined) = (result.layer_a_curve.gains_db.clone(), result.combined_curve.gains_db.clone());
    let height = super::eq_editor::plot_height(ui, 0.3, 160.0, 400.0);
    if let Some(p) = state.output_eq.bus_profile_mut(bus) {
      let background = [
        (tr("Headphone correction"), hp_curve.as_slice(), HP_COLOR, 1.2),
        (tr("Combined"), combined.as_slice(), COMBINED_COLOR, 1.4),
      ];
      changed |= band_editor(ui, "bus_eq", &mut p.bands, MAX_OUT_EQ_BANDS / 2, color, &background, height);
    } else {
      let curves = [
        (tr("Headphone correction"), hp_curve.as_slice(), HP_COLOR, 1.2),
        (tr("Combined (what you hear)"), combined.as_slice(), COMBINED_COLOR, 2.2),
      ];
      curves_plot(ui, "bus_eq_plot", &generate_log_frequencies(), &curves, Some(preamp), height);
    }
    legend(
      ui,
      &[
        (tr("this profile (dots)"), color),
        (tr("headphone correction"), HP_COLOR),
        (tr("combined, what these apps sound like"), COMBINED_COLOR),
      ],
    );
    if preamp != 0.0 {
      ui.label(theme::caption(trf("Volume change {} dB", &[&format!("{preamp:+.1}")])));
    }
    if changed {
      state.apply_output_eq();
      state.out_eq_unsaved = Some(layer);
    }
  });
}
