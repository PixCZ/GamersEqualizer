//! Game tab (game profile with auto-detection, on top of the headphone correction) and the
//! Chat / Media tabs.

use super::eq_editor::{band_editor, curves_plot, profile_picker};
use super::state::{ActiveTab, AppState, OutputLayer};
use super::theme::{self, Severity, icons};
use crate::i18n::{tk, tr, trf};
use crate::output_eq::auto_preamp::generate_log_frequencies;
use crate::output_eq::game_match::{BOUND_NO_PROFILE, MatchMethod};
use crate::sonar_importer::{DEFAULT_SONAR_DB_PATH, import_sonar, read_sonar_sources};
use egui::{Color32, RichText, Ui};
use gamersequalizer_common::biquad::MAX_LAYER_EQ_BANDS;
use gamersequalizer_common::streams::StreamBus;

const GAME_COLOR: Color32 = Color32::from_rgb(82, 183, 136);
const CHAT_COLOR: Color32 = Color32::from_rgb(72, 202, 228);
const MEDIA_COLOR: Color32 = Color32::from_rgb(247, 37, 133);
const HP_COLOR: Color32 = Color32::from_rgb(100, 150, 220);
const LAYER_COLOR: Color32 = Color32::from_rgb(255, 180, 50);
const COMBINED_COLOR: Color32 = Color32::from_rgb(50, 230, 120);
/// Picker value meaning "decide automatically" in the per-app binding combo.
const AUTO: &str = tk("Automatic");
const NOT_A_GAME: &str = tk("Not a game");
/// Picker value meaning "a game that gets no profile of its own" (`BOUND_NO_PROFILE`).
const NO_PROFILE: &str = tk("Game, no profile");

pub fn render_tab_eq(ui: &mut Ui, state: &mut AppState, tab: ActiveTab) {
  match tab {
    ActiveTab::Chat | ActiveTab::Media => render_bus_tab(ui, state, tab),
    _ => render_game_tab(ui, state),
  }
}

fn render_game_tab(ui: &mut Ui, state: &mut AppState) {
  ui.horizontal(|ui| {
    theme::page_header(ui, icons::GAME, GAME_COLOR, tr("Game EQ"), None);
    // Only offered when Sonar's database or GG's preset scripts exist on disk.
    if super::setup::has_sonar_sources(state) {
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let hover = trf(
          "Copies Sonar profiles from {}. If you modified a profile with the same name, your version is backed up to profiles\\_deleted first so it can be restored.",
          &[&DEFAULT_SONAR_DB_PATH],
        );
        if ui.button(theme::with_icon(icons::IMPORT, tr("Import from Sonar DB"))).on_hover_text(hover).clicked() {
          reimport_sonar(state);
        }
      });
    }
  });
  ui.label(
    RichText::new(tr(
      "Game EQ profiles layered on top of your headphone correction, switching automatically based on the active game.",
    ))
    .color(theme::palette(ui).text_secondary),
  );
  ui.add_space(4.0);

  // Switches and detection.
  theme::card(ui, |ui| {
    ui.horizontal_wrapped(|ui| {
      let s = state.output_eq.settings.clone();
      let mut next = s.clone();
      if theme::toggle_labeled(ui, s.game_enabled, tr("Game EQ")) {
        next.game_enabled = !s.game_enabled;
      }
      ui.add_space(16.0);
      ui.checkbox(&mut next.auto_detect, tr("Auto-detect game"))
        .on_hover_text(tr("Automatically switches the EQ profile when a game comes to the foreground and keeps it active until the game closes."));
      ui.add_enabled_ui(next.auto_detect, |ui| {
        ui.checkbox(&mut next.detect_launchers, tr("Launchers"))
          .on_hover_text(tr("Uses local Steam, Epic, GOG, Ubisoft Connect, and Microsoft Store library data on this PC to identify games (e.g. division2.exe → The Division 2)."));
        ui.checkbox(&mut next.detect_names, tr("Names"))
          .on_hover_text(tr("Matches the executable or app name against your profile names (e.g. \"Tom Clancy's The Division 2\")."));
        ui.checkbox(&mut next.detect_genres, tr("Genres"))
          .on_hover_text(tr("Games without a dedicated profile use a genre profile (shooter, MOBA, etc.). Genres are looked up locally from a built-in list, GOG Galaxy, and Steam — nothing is sent online."));
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
    ui.horizontal_wrapped(|ui| {
      ui.label(tr("Profile"));
      let current = state.output_eq.game_profile_name().map(tr).unwrap_or_else(|| tr("None (no game EQ)"));
      ui.label(theme::strong(current, 14.0));
      super::profile_bar::render_output_buttons(ui, state, OutputLayer::Game);
    });
    super::profile_bar::render_output_notice(ui, state, OutputLayer::Game);
    if state.output_eq.game_profile().is_some() {
      ui.add_space(4.0);
      volume_row(ui, state, OutputLayer::Game, None);
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
      changed = band_editor(ui, "game_eq", &mut profile.bands, MAX_LAYER_EQ_BANDS, LAYER_COLOR, &background, height);
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
      ui.label(theme::caption(trf(
        "Volume change {} dB (prevents distortion)",
        &[&format!("{:+.1}", state.output_eq.preamp_db)],
      )));
    }
    if changed {
      state.apply_output_eq();
      state.out_eq_unsaved = Some(OutputLayer::Game);
    }
  });

  theme::card(ui, |ui| {
    egui::CollapsingHeader::new(theme::strong(tr("Recent apps: assign a profile manually"), 14.0))
      .show(ui, |ui| recent_apps(ui, state));
  });
}

/// Colored line samples with their meaning, under a graph.
fn legend(ui: &mut Ui, items: &[(&str, Color32)]) {
  ui.horizontal_wrapped(|ui| {
    for (idx, &(text, color)) in items.iter().enumerate() {
      let c = theme::adapt(ui, color);
      let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 12.0), egui::Sense::hover());
      let y = rect.center().y;
      let painter = ui.painter();
      match idx {
        // 0 = edited curve (thick + dot), 1 = headphone correction (dashed), 2+ = combined (solid).
        0 => {
          painter
            .line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], egui::Stroke::new(2.5_f32, c));
          painter.circle_filled(rect.center(), 3.0, c);
        }
        1 => {
          let stroke = egui::Stroke::new(1.5_f32, c);
          painter.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.left() + 6.0, y)], stroke);
          painter.line_segment([egui::pos2(rect.right() - 6.0, y), egui::pos2(rect.right(), y)], stroke);
        }
        _ => {
          painter
            .line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], egui::Stroke::new(1.8_f32, c));
        }
      }
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
        let exe = state.output_eq.active_game_exe(&game.label);
        ui.label(tr("→ profile:"));
        let current = (!game.profile.is_empty()).then_some(game.profile.as_str());
        if let Some(exe) = exe {
          if let Some(choice) =
            profile_picker(ui, "active_game_profile", current, &refs, tr("(none)"), &mut filter, 240.0)
          {
            state.out_eq_unsaved = None;
            // "(none)": still a game, without a profile of its own. Not "" (that is "Not a game")
            // and not an unbound exe: detection would pick the same profile again.
            let binding = choice.unwrap_or_else(|| BOUND_NO_PROFILE.to_string());
            state.output_eq_do(|eq, shm| eq.bind_exe(&exe, Some(binding), shm));
          }
        } else {
          ui.label(theme::strong(current.map_or(tr("none"), tr), 14.0));
        }
        if ui
          .button(tr("Reset"))
          .on_hover_text(tr("Forget the detected game until it comes to the foreground again"))
          .clicked()
        {
          state.out_eq_unsaved = None;
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
      state.out_eq_unsaved = None;
      state.output_eq.settings.fallback_game_profile = choice;
      state.output_eq_do(|eq, shm| eq.save_settings(shm));
    }
  });
  state.headphones_ui.filter = filter;
}

fn recent_apps(ui: &mut Ui, state: &mut AppState) {
  ui.label(theme::caption(tr(
    "Apps that recently ran in the foreground. Assign a profile if a game wasn't detected, or mark an app as not a game.",
  )));
  let (auto, not_a_game, no_profile) = (tr(AUTO), tr(NOT_A_GAME), tr(NO_PROFILE));
  let mut names: Vec<String> = vec![auto.to_string(), not_a_game.to_string(), no_profile.to_string()];
  names.extend(state.output_eq.game_profiles.iter().map(|p| p.name.clone()));
  let refs: Vec<&str> = names.iter().map(String::as_str).collect();
  let apps = state.output_eq.recent_apps.clone();
  if apps.is_empty() {
    ui.label(theme::caption(tr("Switch to a game window once, then come back here.")));
  }
  let mut filter = std::mem::take(&mut state.headphones_ui.filter);
  egui::Grid::new("recent_apps").num_columns(3).spacing([16.0, 8.0]).show(ui, |ui| {
    for (i, app) in apps.iter().enumerate() {
      ui.label(theme::strong(&app.label, 14.0));
      let how = match app.matched.as_ref().map(|m| (m.method, m.profile.as_deref())) {
        Some((MatchMethod::Binding, Some(p))) => trf("assigned: {}", &[&tr(p)]),
        Some((MatchMethod::LauncherId, Some(p))) => trf("launcher id: {}", &[&tr(p)]),
        Some((MatchMethod::Name, Some(p))) => trf("by name: {}", &[&tr(p)]),
        Some((MatchMethod::Genre, Some(p))) => trf("by genre: {}", &[&tr(p)]),
        Some((_, _)) => tr("game, no profile").to_string(),
        None => tr("not a game").to_string(),
      };
      ui.label(theme::caption(how));
      let bound = state.output_eq.settings.bindings.get(&app.exe_name).cloned();
      let current = match bound.as_deref() {
        None => auto,
        Some("") => not_a_game,
        Some(BOUND_NO_PROFILE) => no_profile,
        Some(p) => p,
      };
      if let Some(Some(choice)) =
        profile_picker(ui, &format!("bind_{i}"), Some(current), &refs, auto, &mut filter, 220.0)
      {
        let binding = match choice.as_str() {
          c if c == auto => None,
          c if c == not_a_game => Some(String::new()),
          c if c == no_profile => Some(BOUND_NO_PROFILE.to_string()),
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

/// Volume of the shown profile, the loudness-matching switch, and the resulting volume change.
/// Used by the Headphones, Game, Chat and Media tabs.
/// `bus_volume`: (volume change, peak) of a chat / media channel, which its tab has already
/// computed for the graph; None = the output's own.
pub fn volume_row(ui: &mut Ui, state: &mut AppState, layer: OutputLayer, bus_volume: Option<(f32, f32)>) {
  ui.horizontal_wrapped(|ui| {
    ui.label(tr("Volume of this profile"));
    let profile = state.output_eq.layer_profile_mut(layer);
    let mut changed = false;
    if let Some(p) = profile {
      changed = theme::slider(ui, egui::Slider::new(&mut p.gain_db, -12.0..=12.0).suffix(" dB").max_decimals(1))
        .on_hover_text(tr("Adjust this profile's volume by ear — for example, to match loudness across different headphones. Saved with the profile."))
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
        "EQ boosts need headroom to avoid distortion, so the app lowers the volume first (preamp).\nON: lowers volume based on the average level between 200 Hz and 6 kHz, keeping loudness consistent when switching profiles. Rare peaks above maximum (up to +3 dB) are smoothly caught by a limiter.\nOFF: lowers volume by the highest boost. Never triggers the limiter, but profiles with heavy boosts sound quieter.",
      ))
      .changed()
    {
      state.output_eq.settings.loudness_match = matching;
      state.output_eq_do(|eq, shm| eq.save_settings(shm));
    }
  });

  let (preamp, peak) = bus_volume.unwrap_or((state.output_eq.preamp_db, state.output_eq.peak_after_db));
  let mut summary = trf("Total volume change: {} dB.", &[&format!("{preamp:+.1}")]);
  if peak > 0.05 {
    summary.push(' ');
    summary.push_str(&trf(
      "Strongest EQ peaks reach {} dB above full scale; the limiter turns them down.",
      &[&format!("{peak:+.1}")],
    ));
  }
  ui.label(theme::caption(summary));
}

/// Copies the SteelSeries Sonar profiles in (the button here, the offer on the setup card).
pub(super) fn reimport_sonar(state: &mut AppState) {
  match read_sonar_sources() {
    Ok(profiles) => {
      let stats = import_sonar(&profiles, &state.storage);
      log::info!("Sonar import: {} profiles", stats.total_imported);
      state.available_mic_profiles = super::mic_presets::all_mic_presets(&state.storage);
      state.output_eq_do(|eq, shm| {
        eq.reload();
        eq.settings.imported_sonar = true;
        eq.save_settings(shm);
      });
    }
    Err(e) => log::error!("Sonar import failed: {e}"),
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
      if theme::toggle_labeled(ui, on, title) {
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
        Some(n) => (Severity::Success, trf("Active on {} app stream(s)", &[&n])),
      };
      theme::status_line(ui, severity, &status).on_hover_text(tr(
        "Number of active audio streams matched to apps on this channel. Streams are detected within about a second once audio starts playing; silent streams are ignored.",
      ));
    });
    ui.label(theme::caption(tr(
      "Applied per app before Windows mixes audio together — no virtual sound card and no extra latency. Install the audio effects on this output using the banner at the top. Unmatched apps keep headphone + game EQ.",
    )));
    ui.add_space(4.0);

    // Profile picker + shared profile actions; none = flat (headphone correction only).
    let names: Vec<String> = state.output_eq.bus_profiles(bus).iter().map(|p| p.name.clone()).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut filter = std::mem::take(&mut state.headphones_ui.filter);
    ui.horizontal_wrapped(|ui| {
      ui.label(tr("Profile"));
      let current = state.output_eq.bus_profile_name(bus).map(str::to_string);
      if let Some(choice) = profile_picker(
        ui,
        "bus_profile",
        current.as_deref(),
        &refs,
        tr("Flat (headphone correction only)"),
        &mut filter,
        260.0,
      ) {
        state.out_eq_unsaved = None;
        state.output_eq_do(|eq, shm| eq.select_bus_profile(bus, choice, shm));
      }
      super::profile_bar::render_output_buttons(ui, state, layer);
    });
    state.headphones_ui.filter = filter;
    super::profile_bar::render_output_notice(ui, state, layer);
  });

  theme::card(ui, |ui| {
    let (hp_curve, combined, preamp, peak) = {
      let (result, preamp, peak) = state.output_eq.bus_result(bus);
      (result.layer_a_curve.gains_db.clone(), result.combined_curve.gains_db.clone(), preamp, peak)
    };
    if state.output_eq.bus_profile(bus).is_some() {
      volume_row(ui, state, layer, Some((preamp, peak)));
      ui.add_space(4.0);
    }
    let mut changed = false;

    // Graph: this profile (editable dots) over the headphone correction and the sum.
    let height = super::eq_editor::plot_height(ui, 0.3, 160.0, 400.0);
    if let Some(p) = state.output_eq.bus_profile_mut(bus) {
      let background = [
        (tr("Headphone correction"), hp_curve.as_slice(), HP_COLOR, 1.2),
        (tr("Combined"), combined.as_slice(), COMBINED_COLOR, 1.4),
      ];
      changed |= band_editor(ui, "bus_eq", &mut p.bands, MAX_LAYER_EQ_BANDS, color, &background, height);
    } else {
      let curves = [
        (tr("Headphone correction"), hp_curve.as_slice(), HP_COLOR, 1.2),
        (tr("Combined (what you hear)"), combined.as_slice(), COMBINED_COLOR, 2.2),
      ];
      curves_plot(ui, "bus_eq_plot", &generate_log_frequencies(), &curves, Some(preamp), height);
      ui.label(theme::caption(tr("No profile selected (flat). Pick one above or click \"New…\" to create one.")));
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
