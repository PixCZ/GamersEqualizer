//! Headphones tab: one-click switching between the user's headphones (the choice is
//! remembered per output device), an ON/OFF switch to compare, the status of the output APO
//! that applies the correction, and a step-by-step wizard that gets an AutoEQ profile into
//! the app without the app going online: it says where to click, finds the downloaded file
//! and checks it before saving.

use super::eq_editor::{band_editor, response_plot};
use super::state::{AppState, OutputLayer};
use super::theme::{self, Severity, icons};
use crate::i18n::{tr, trf};
use crate::output_eq::autoeq::{self, AutoEqEntry, Check, CheckLevel, DownloadsScan, ParsedEq};
use crate::win_audio::apo_install::{self, ApoStatus, Flow};
use crate::win_audio::{WinAudioSystem, shell};
use egui::{Color32, Ui};
use gamersequalizer_common::biquad::MAX_LAYER_EQ_BANDS;
use gamersequalizer_common::profile::{ParametricProfile, VadType};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

mod autoeq_wizard;

use autoeq_wizard::*;

const COLOR: Color32 = Color32::from_rgb(100, 150, 220);
const APO_RECHECK: Duration = Duration::from_secs(3);

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
      super::tab_eq::volume_row(ui, state, OutputLayer::Headphone, None);
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
    if theme::toggle_labeled(ui, on, tr("Headphone EQ")) {
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
