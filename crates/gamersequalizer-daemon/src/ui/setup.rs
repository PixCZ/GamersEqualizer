//! One card for every change the app makes in Windows. It opens by itself on the first start,
//! and whenever a fix needs administrator rights (the health bar, the Headphones tab).
//!
//! Like Android's permission rationale: it says what will change, why Windows asks and what
//! the user will notice, and only then asks Windows. All admin steps (mic chain on the
//! microphone, output EQ on the output) run in one elevated installer: one UAC prompt and one
//! Windows audio restart, however many devices. Steps without admin rights (start with
//! Windows) are done right away.

use std::time::{Duration, Instant};

use egui::Ui;

use super::state::AppState;
use super::theme::{self, Severity};
use crate::i18n::{tr, trf};
use crate::win_audio::apo_install::{self, ApoCheck, ApoStatus, Flow};
use crate::win_audio::WinAudioSystem;

/// Registry + file metadata only, like the health bar.
const RECHECK_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Default)]
pub struct SetupUi {
  open: bool,
  /// Ticks the user changed from the default.
  choices: Vec<(Kind, bool)>,
  /// Output EQ on the Master output, its name, and whether it is on any output at all.
  output: Option<ApoCheck>,
  output_name: String,
  output_anywhere: bool,
  checked_at: Option<Instant>,
  /// Installer started: the card waits for it and closes when nothing is left.
  launched: bool,
  error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
  Mic,
  Output,
  Autostart,
}

struct Item {
  kind: Kind,
  title: String,
  detail: &'static str,
  /// Endpoint GUID for an APO step (needs administrator rights); None for the others.
  admin_target: Option<(Flow, String)>,
}

/// Opens the card (a fix button somewhere asked for a system change).
pub fn open(state: &mut AppState) {
  let setup = &mut state.setup;
  setup.open = true;
  setup.launched = false;
  setup.error = None;
  setup.checked_at = None;
}

/// Re-reads the output EQ state every few seconds. Call once per frame after
/// `apo_banner::refresh` (the mic state comes from there).
pub fn refresh(state: &mut AppState, win_audio: &WinAudioSystem) {
  if state.setup.checked_at.is_some_and(|t| t.elapsed() < RECHECK_INTERVAL) {
    return;
  }
  let output_id = win_audio.chatmix.master_device_id.clone();
  let check = apo_install::check_flow(Flow::Render, output_id.as_deref());
  state.setup.output_name = state
    .endpoints
    .iter()
    .find(|e| Some(&e.id) == output_id.as_ref())
    .map(|e| e.name.clone())
    .unwrap_or_else(|| tr("Default output").to_string());
  state.setup.output_anywhere = apo_install::installed_on_any(Flow::Render);
  state.setup.output = Some(check);
  state.setup.checked_at = Some(Instant::now());

  if super::apo_banner::mic_check(state).is_none() {
    return; // Not checked yet: no verdict.
  }
  let nothing_left = items(state).is_empty();
  if nothing_left && !state.prefs.setup_done {
    // Set up before (older version, scripts) or nothing to do here: never show the card.
    finish(state);
  }
  if nothing_left && state.setup.launched {
    state.setup.open = false;
    state.setup.launched = false;
  }
}

/// The output in use lacks the output EQ that other outputs have (a new sound card or headset).
pub fn output_needs_setup(state: &AppState) -> Option<&str> {
  let check = state.setup.output.as_ref()?;
  (check.status == ApoStatus::NotOnThisMic && state.setup.output_anywhere && check.can_install)
    .then_some(state.setup.output_name.as_str())
}

pub fn visible(state: &AppState) -> bool {
  state.setup.open
    || (!state.prefs.setup_done && super::apo_banner::mic_check(state).is_some() && !items(state).is_empty())
}

pub fn render(ui: &mut Ui, state: &mut AppState) {
  let list = items(state);
  let first_run = !state.prefs.setup_done;
  let mut apply = false;
  let mut close = false;
  theme::info_bar(ui, Severity::Info, |ui| {
    let title = if first_run { tr("Set up GamersEqualizer") } else { tr("Changes in Windows") };
    ui.label(theme::strong(title, 15.0));
    if state.setup.launched {
      ui.label(tr(
        "Installer started: confirm the Windows prompt. This card closes by itself when everything is set up.",
      ));
      close = ui.button(tr("Close")).clicked();
      return;
    }
    if list.is_empty() {
      ui.label(tr("Everything is set up."));
      close = ui.button(tr("Close")).clicked();
      return;
    }
    ui.label(tr(
      "Pick what to set up. Everything that needs administrator rights is done in one go: Windows asks once, \
       and the sound stops once, for a few seconds.",
    ));
    ui.add_space(4.0);
    for item in &list {
      let mut on = ticked(state, item);
      if ui.checkbox(&mut on, item.title.as_str()).changed() {
        set_ticked(state, item.kind, on);
      }
      let rights = if item.admin_target.is_some() {
        tr("Needs administrator rights.")
      } else {
        tr("No administrator rights needed.")
      };
      ui.indent(("setup_item", item.kind as u8), |ui| {
        ui.label(theme::caption(format!("{} {rights}", item.detail)));
      });
    }
    let admin = list.iter().any(|i| i.admin_target.is_some() && ticked(state, i));
    if admin {
      ui.add_space(4.0);
      theme::inset_frame(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(theme::strong(tr("Why does this need administrator rights?"), 14.0));
        ui.label(tr(
          "The effects run inside Windows audio itself, so every app (Discord, games, the browser) gets them \
           without any setup in that app, and no virtual sound card is needed. They are installed for the whole \
           computer (Program Files and the system registry), so Windows asks for permission.\n\
           What happens: one Windows prompt, a small installer window, and Windows audio restarts for a few \
           seconds. The app itself is copied next to the effects and continues from there (the window reopens), \
           so the folder you started it from can be deleted afterwards. Apps that use the microphone or the \
           headphones may need to be reopened.\n\
           Nothing else is changed. Settings > Uninstall from Windows removes all of it.",
        ));
      });
    }
    // The install restarts Windows audio: a call loses the mic and stays silent until it is
    // rejoined (tester report: installed during a call, silent even after restarting this app).
    let recording = super::tab_mixer::recording_apps_cached();
    if admin && !recording.is_empty() {
      ui.add_space(4.0);
      let apps = crate::health::join_apps(&recording);
      theme::status_line(
        ui,
        Severity::Warning,
        &trf(
          "{} is using the microphone right now (a call?). The install restarts Windows audio: the call loses \
           the microphone and stays silent until you rejoin it or restart {}. Better finish the call first.",
          &[&apps, &apps],
        ),
      );
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
      let label = if admin { tr("Continue – ask Windows") } else { tr("Apply") };
      apply = ui.add(theme::accent_button(ui, label)).clicked();
      close = ui.button(tr("Not now")).clicked();
    });
    if let Some(e) = state.setup.error.as_deref() {
      theme::status_line(ui, Severity::Error, &trf("Setup failed: {}", &[&e]));
    }
  });
  ui.add_space(6.0);

  if apply {
    run(state, &list);
  } else if close {
    state.setup.open = false;
    state.setup.launched = false;
    if first_run {
      // "Not now" on the first start: the health bar keeps offering the fixes.
      finish(state);
    }
  }
}

fn run(state: &mut AppState, list: &[Item]) {
  let chosen: Vec<&Item> = list.iter().filter(|i| ticked(state, i)).collect();
  if chosen.iter().any(|i| i.kind == Kind::Autostart) {
    match crate::autostart::set_autostart(true) {
      Ok(()) => state.autostart_enabled = true,
      Err(e) => log::warn!("[Setup] autostart failed: {e}"),
    }
  }
  let targets: Vec<(Flow, &str)> =
    chosen.iter().filter_map(|i| i.admin_target.as_ref()).map(|(flow, guid)| (*flow, guid.as_str())).collect();
  if !targets.is_empty() {
    if let Err(e) = apo_install::launch_install_many(&targets) {
      log::warn!("[Setup] install could not start: {e}");
      state.setup.error = Some(e);
      return;
    }
    log::info!("[Setup] installer started for {} device(s)", targets.len());
    state.setup.launched = true;
    state.setup.open = true;
  } else {
    state.setup.open = false;
  }
  state.setup.error = None;
  finish(state);
  super::apo_banner::recheck(state);
  state.setup.checked_at = None;
}

fn finish(state: &mut AppState) {
  if !state.prefs.setup_done {
    state.prefs.setup_done = true;
    state.prefs.save();
  }
}

/// What is left to set up, in the order shown.
fn items(state: &AppState) -> Vec<Item> {
  let mut list = Vec::new();
  if let Some(check) = super::apo_banner::mic_check(state) {
    if let Some(guid) = needs_install(check) {
      let title = match check.status {
        ApoStatus::Outdated => tr("Update GamersEqualizer in Windows").to_string(),
        _ => trf("Microphone processing on \"{}\"", &[&super::apo_banner::mic_in_use_name(state)]),
      };
      list.push(Item {
        kind: Kind::Mic,
        title,
        detail: tr("Echo cancellation, noise suppression and EQ for Discord, games and every other app."),
        admin_target: Some((Flow::Capture, guid)),
      });
    }
  }
  if let Some(guid) = state.setup.output.as_ref().and_then(needs_install) {
    list.push(Item {
      kind: Kind::Output,
      title: trf("Output EQ on \"{}\"", &[&state.setup.output_name]),
      detail: tr("Headphone correction and the game, chat and media EQ on what you hear."),
      admin_target: Some((Flow::Render, guid)),
    });
  }
  if !state.prefs.setup_done && !state.autostart_enabled {
    list.push(Item {
      kind: Kind::Autostart,
      title: tr("Start with Windows").to_string(),
      detail: tr("The background service starts when you sign in, so the effects get their settings."),
      admin_target: None,
    });
  }
  list
}

/// Endpoint GUID when this APO check calls for the installer.
fn needs_install(check: &ApoCheck) -> Option<String> {
  let wanted = matches!(check.status, ApoStatus::NotInstalled | ApoStatus::Outdated | ApoStatus::NotOnThisMic);
  check.endpoint_guid.clone().filter(|_| wanted && check.can_install)
}

fn ticked(state: &AppState, item: &Item) -> bool {
  if let Some((_, on)) = state.setup.choices.iter().find(|(k, _)| *k == item.kind) {
    return *on;
  }
  // The output EQ is optional: offered ticked on the first start or where it is used already,
  // unticked when the user declined it before (it is on no output yet).
  item.kind != Kind::Output || !state.prefs.setup_done || state.setup.output_anywhere
}

fn set_ticked(state: &mut AppState, kind: Kind, on: bool) {
  state.setup.choices.retain(|(k, _)| *k != kind);
  state.setup.choices.push((kind, on));
}
