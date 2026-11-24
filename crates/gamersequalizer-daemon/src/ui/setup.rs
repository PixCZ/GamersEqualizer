//! One card for every change the app makes in Windows. It opens by itself on the first start,
//! and whenever a fix needs administrator rights (the health bar, the Headphones tab).
//!
//! Like Android's permission rationale: it says what will change, why Windows asks and what
//! the user will notice, and only then asks Windows. All admin steps (mic chain on the
//! microphone, output EQ on the output) run in one elevated installer: one UAC prompt and one
//! Windows audio restart, however many devices. Steps without admin rights (start with
//! Windows) are done right away.
//!
//! On the first start it also offers to copy the SteelSeries Sonar profiles in (ticked) when
//! SteelSeries GG is on this PC.
//!
//! By default only the devices in use get the effects; other connected devices are offered
//! unticked (each costs extra audio restarts if its check fails). A device the installer rolled
//! back is not offered again until the user clicks "Try again".

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use egui::Ui;

use super::state::AppState;
use super::theme::{self, Severity};
use crate::i18n::{tr, trf};
use crate::win_audio::WinAudioSystem;
use crate::win_audio::apo_install::{self, ApoCheck, ApoStatus, Flow};

/// Registry + file metadata only, like the health bar.
const RECHECK_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Default)]
pub struct SetupUi {
  open: bool,
  /// Ticks the user changed from the default.
  choices: Vec<(Kind, bool)>,
  /// Output EQ on the Master output, its name, and whether effects are on any output at all.
  output: Option<ApoCheck>,
  output_name: String,
  output_anywhere: bool,
  /// Other active physical devices without our effect, `(flow, guid, name)`: offered unticked.
  extras: Vec<(Flow, String, String)>,
  /// `apo_install::endpoint_key` of the extras the user ticked.
  extras_ticked: Vec<String>,
  /// Connected devices the installer rolled back, `(endpoint_key, name)`.
  rolled_back: Vec<(String, String)>,
  checked_at: Option<Instant>,
  /// Installer started: the card waits for it and closes when nothing is left.
  launched: bool,
  error: Option<String>,
  /// Profiles in the SteelSeries Sonar database; read once (None = not yet).
  sonar_profiles: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
  Mic,
  Output,
  Autostart,
  SonarImport,
}

struct Item {
  kind: Kind,
  title: String,
  detail: String,
  /// Endpoint GUIDs for an APO step (needs administrator rights); empty for the others.
  admin_targets: Vec<(Flow, String)>,
}

/// Opens the card (a fix button somewhere asked for a system change).
pub fn open(state: &mut AppState) {
  let setup = &mut state.setup;
  setup.open = true;
  setup.launched = false;
  setup.error = None;
  setup.checked_at = None;
}

/// Opens the card from the Headphones tab with Output EQ pre-ticked even if declined before.
pub fn open_for_output(state: &mut AppState) {
  open(state);
  set_ticked(state, Kind::Output, true);
}

/// Profiles Sonar's database and GG's preset scripts hold (0 without SteelSeries GG). Read once
/// per run.
/// shortcut: GG installed while the app runs is seen after a restart; recount on the device
/// recheck if that matters.
pub fn sonar_profiles(state: &mut AppState) -> usize {
  *state.setup.sonar_profiles.get_or_insert_with(|| match crate::sonar_importer::read_sonar_sources() {
    Ok(p) => {
      log::info!("[Setup] SteelSeries Sonar: {} profiles", p.len());
      p.len()
    }
    Err(e) => {
      log::warn!("[Setup] SteelSeries Sonar profiles not readable: {e}");
      0
    }
  })
}

/// Check again on the next frame (active device or endpoint list changed).
pub fn recheck(state: &mut AppState) {
  state.setup.checked_at = None;
}

/// Re-reads the output EQ state every few seconds. Call once per frame after
/// `apo_banner::refresh` (the mic state comes from there).
pub fn refresh(state: &mut AppState, win_audio: &WinAudioSystem) {
  if state.setup.checked_at.is_some_and(|t| t.elapsed() < RECHECK_INTERVAL) {
    return;
  }
  take_rollbacks(state);
  sonar_profiles(state);
  let output_id = win_audio.chatmix.master_device_id.clone();
  let check = apo_install::check_flow(Flow::Render, output_id.as_deref());
  state.setup.output_name = state
    .endpoints
    .iter()
    .find(|e| Some(&e.id) == output_id.as_ref())
    .map(|e| e.name.clone())
    .unwrap_or_else(|| tr("Default output").to_string());
  state.setup.output_anywhere = apo_install::installed_on_any(Flow::Render);
  let mic_guid = super::apo_banner::mic_check(state).and_then(|c| c.endpoint_guid.clone());
  let in_use = [(Flow::Capture, mic_guid), (Flow::Render, check.endpoint_guid.clone())];
  let mut extras = Vec::new();
  for (flow, in_use_guid) in in_use {
    let endpoints = if flow == Flow::Capture { &state.capture_endpoints } else { &state.endpoints };
    for (guid, name) in apo_install::targets_to_install(flow, endpoints) {
      let used = in_use_guid.as_deref().is_some_and(|g| g.eq_ignore_ascii_case(&guid));
      if !used && !state.prefs.rolled_back.contains(&apo_install::endpoint_key(flow, &guid)) {
        extras.push((flow, guid, name));
      }
    }
  }
  state.setup.extras_ticked.retain(|key| extras.iter().any(|(f, g, _)| apo_install::endpoint_key(*f, g) == *key));
  state.setup.extras = extras;
  state.setup.rolled_back =
    state.prefs.rolled_back.iter().filter_map(|key| Some((key.clone(), connected_name(state, key)?))).collect();
  state.setup.output = Some(check);
  state.setup.checked_at = Some(Instant::now());

  if super::apo_banner::mic_check(state).is_none() {
    return; // Not checked yet: no verdict.
  }
  let nothing_left = items(state).is_empty();
  if nothing_left && !state.prefs.setup_done {
    // Set up before (older version, scripts) or nothing to do here: never show the card.
    finish_setup(state, None);
  }
  if nothing_left && state.setup.launched {
    state.setup.open = false;
    state.setup.launched = false;
  }
}

/// The output in use lacks our output effect: another output has it (a new sound card or headset),
/// or echo cancellation is on, which takes its speaker reference from this effect.
pub fn output_needs_setup(state: &AppState) -> Option<&str> {
  let check = state.setup.output.as_ref()?;
  let aec_on = state.shm_header().is_some_and(|h| h.aec_enabled.load(Ordering::Relaxed));
  needs_output_setup(&check.status, check.can_install, state.setup.output_anywhere, aec_on)
    .then_some(state.setup.output_name.as_str())
}

fn needs_output_setup(status: &ApoStatus, can_install: bool, output_anywhere: bool, aec_on: bool) -> bool {
  *status == ApoStatus::NotOnThisMic && can_install && (output_anywhere || aec_on)
}

pub fn visible(state: &AppState) -> bool {
  state.setup.open
    || (!state.prefs.setup_done && super::apo_banner::mic_check(state).is_some() && !items(state).is_empty())
}

pub fn render(ui: &mut Ui, state: &mut AppState) {
  let list = items(state);
  let extras = state.setup.extras.clone();
  let rolled_back = state.setup.rolled_back.clone();
  let first_run = !state.prefs.setup_done;
  let mut apply = false;
  let mut close = false;
  let mut retry = None;
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
    if list.is_empty() && extras.is_empty() && rolled_back.is_empty() {
      ui.label(tr("Everything is set up."));
      close = ui.button(tr("Close")).clicked();
      return;
    }
    ui.label(tr(
      "Choose what to set up. Everything requiring administrator rights runs in one step — Windows asks once and audio stops once, for a few seconds.",
    ));
    ui.add_space(4.0);
    for item in &list {
      let mut on = ticked(state, item);
      if ui.checkbox(&mut on, item.title.as_str()).changed() {
        set_ticked(state, item.kind, on);
      }
      let rights = if item.admin_targets.is_empty() {
        tr("No administrator rights needed.")
      } else {
        tr("Needs administrator rights.")
      };
      ui.indent(("setup_item", item.kind as u8), |ui| {
        ui.label(theme::caption(format!("{} {rights}", item.detail)));
      });
    }
    if !extras.is_empty() {
      ui.add_space(4.0);
      ui.label(theme::strong(tr("Other connected devices"), 15.0));
      ui.label(theme::caption(tr(
        "Currently unused devices. Check any you also want to set up. Each device is verified after installation (if a check fails, Windows audio restarts once more to undo it). Requires administrator rights.",
      )));
      for (flow, guid, name) in &extras {
        let key = apo_install::endpoint_key(*flow, guid);
        let mut on = state.setup.extras_ticked.contains(&key);
        let label = match flow {
          Flow::Capture => trf("Microphone processing on \"{}\"", &[name]),
          Flow::Render => trf("Output EQ on \"{}\"", &[name]),
        };
        if ui.checkbox(&mut on, label.as_str()).changed() {
          state.setup.extras_ticked.retain(|k| *k != key);
          if on {
            state.setup.extras_ticked.push(key);
          }
        }
      }
    }
    for (key, name) in &rolled_back {
      ui.add_space(4.0);
      ui.horizontal_wrapped(|ui| {
        ui.label(theme::caption(trf(
          "Not offered: the installer reverted its change on \"{}\" because the post-install check failed (see the Log tab for details).",
          &[name],
        )));
        if ui.button(tr("Try again")).clicked() {
          retry = Some(key.clone());
        }
      });
    }
    let admin =
      list.iter().any(|i| !i.admin_targets.is_empty() && ticked(state, i)) || !state.setup.extras_ticked.is_empty();
    if admin {
      ui.add_space(4.0);
      theme::inset_frame(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(theme::strong(tr("Why does this need administrator rights?"), 15.0));
        ui.label(tr(
          "Audio effects run directly inside Windows audio, so every app (Discord, games, browsers) gets them automatically with no virtual sound card or per-app setup. Because they install system-wide (Program Files and system registry), Windows asks for administrator permission.\nWhat happens: one Windows prompt, the installer window opens briefly, and Windows audio restarts for a few seconds. The app copies itself next to the effects and reopens from there, so you can delete the original folder afterward. Apps currently using your mic or headphones may need a restart.\nNothing else is changed, and Settings > Uninstall from Windows removes everything cleanly.",
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
          "{} is currently using the microphone. Installing restarts Windows audio, which will drop the microphone until you rejoin the call or restart {}. It is best to finish your call first.",
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

  if let Some(key) = retry {
    log::info!("[Setup] {key} is offered again (\"Try again\" after a rollback)");
    state.prefs.rolled_back.retain(|k| *k != key);
    state.prefs.save();
    state.setup.checked_at = None;
  }
  if apply {
    run(state, &list);
  } else if close {
    state.setup.open = false;
    state.setup.launched = false;
    if first_run {
      // "Not now" on the first start: the health bar keeps offering the fixes.
      finish_setup(state, None);
    }
  }
}

fn run(state: &mut AppState, list: &[Item]) {
  let chosen: Vec<&Item> = list.iter().filter(|i| ticked(state, i)).collect();
  // Before the installer: its copy of the app starts with the profiles already there.
  if chosen.iter().any(|i| i.kind == Kind::SonarImport) {
    super::tab_eq::reimport_sonar(state);
  }
  if chosen.iter().any(|i| i.kind == Kind::Autostart) {
    match crate::autostart::set_autostart(true) {
      Ok(()) => state.autostart_enabled = true,
      Err(e) => log::warn!("[Setup] autostart failed: {e}"),
    }
  }
  let mut targets: Vec<(Flow, &str)> =
    chosen.iter().flat_map(|i| i.admin_targets.iter().map(|(flow, guid)| (*flow, guid.as_str()))).collect();
  let ticked_extra = |(flow, guid, _): &&(Flow, String, String)| {
    state.setup.extras_ticked.contains(&apo_install::endpoint_key(*flow, guid))
  };
  targets.extend(state.setup.extras.iter().filter(ticked_extra).map(|(flow, guid, _)| (*flow, guid.as_str())));
  if !targets.is_empty() {
    if let Err(e) = apo_install::launch_install_many(&targets) {
      log::warn!("[Setup] install could not start: {e}");
      state.setup.error = Some(e);
      return;
    }
    log::info!("[Setup] installer started for {} device(s)", targets.len());
    state.setup.extras_ticked.clear();
    state.setup.launched = true;
    state.setup.open = true;
  } else {
    state.setup.open = false;
  }
  state.setup.error = None;
  finish_setup(state, Some(list));
  super::apo_banner::recheck(state);
  state.setup.checked_at = None;
}

fn finish_setup(state: &mut AppState, list: Option<&[Item]>) {
  let mut save = !state.prefs.setup_done;
  state.prefs.setup_done = true;
  if let Some(out_item) = list.and_then(|items| items.iter().find(|i| i.kind == Kind::Output)) {
    let declined = !ticked(state, out_item);
    if state.prefs.output_declined != declined {
      state.prefs.output_declined = declined;
      save = true;
    }
  }
  if save {
    state.prefs.save();
  }
}

/// What is left to set up on the devices in use, in the order shown (other devices: `extras`).
/// A device the installer rolled back is left out until "Try again".
fn items(state: &AppState) -> Vec<Item> {
  let mut list = Vec::new();
  let offered = |flow: Flow, guid: &String| !state.prefs.rolled_back.contains(&apo_install::endpoint_key(flow, guid));
  let mic = super::apo_banner::mic_check(state);
  if let Some((check, guid)) = mic.and_then(|c| Some((c, needs_install(c).filter(|g| offered(Flow::Capture, g))?))) {
    let title = if check.status == ApoStatus::Outdated {
      tr("Update GamersEqualizer in Windows").to_string()
    } else {
      trf("Microphone processing on \"{}\"", &[&super::apo_banner::mic_in_use_name(state)])
    };
    let detail = tr("Echo cancellation, noise suppression and EQ for Discord, games and every other app.").to_string();
    list.push(Item { kind: Kind::Mic, title, detail, admin_targets: vec![(Flow::Capture, guid)] });
  }
  if let Some(guid) = state.setup.output.as_ref().and_then(needs_install).filter(|g| offered(Flow::Render, g)) {
    let title = trf("Output EQ on \"{}\"", &[&state.setup.output_name]);
    let detail =
      tr("Headphone correction and the game, chat and media EQ; echo cancellation needs it too.").to_string();
    list.push(Item { kind: Kind::Output, title, detail, admin_targets: vec![(Flow::Render, guid)] });
  }
  if !state.prefs.setup_done && !state.autostart_enabled {
    list.push(Item {
      kind: Kind::Autostart,
      title: tr("Start with Windows").to_string(),
      detail: tr("The background service starts when you sign in, so the effects get their settings.").to_string(),
      admin_targets: Vec::new(),
    });
  }
  let sonar = state.setup.sonar_profiles.unwrap_or(0);
  if !state.prefs.setup_done && !state.output_eq.settings.imported_sonar && sonar > 0 {
    list.push(Item {
      kind: Kind::SonarImport,
      title: trf("Import {} profiles from SteelSeries Sonar", &[&sonar]),
      detail: tr(
        "SteelSeries GG was found on this PC. Its game profiles and microphone presets will be copied here without changing SteelSeries GG. You can re-import anytime from the Game tab.",
      )
      .to_string(),
      admin_targets: Vec::new(),
    });
  }
  list
}

/// New rollback lines of the installer (in the service's notes) go into the prefs, so the card
/// stops offering those devices.
/// shortcut: only the notes the window gets (`run_status.json`, newest 40) are seen; a rollback
/// line pushed out of them during a long multi-device install is missed. Upgrade: the installer
/// writes the rolled-back endpoints to a file in Program Files if that happens.
fn take_rollbacks(state: &mut AppState) {
  let Some(run) = super::apo_banner::run_status(state) else {
    return;
  };
  // Times to the second (`rolled_back_endpoint`), so `>=`: another rollback in the second seen
  // last still counts; one already taken is skipped by the key check.
  let seen = &state.prefs.rolled_back_seen;
  let found: Vec<(String, String)> = run
    .apo_notes
    .iter()
    .filter_map(|n| apo_install::rolled_back_endpoint(n))
    .filter(|(time, _)| time >= seen)
    .collect();
  let mut changed = false;
  for (time, key) in found {
    if time > state.prefs.rolled_back_seen {
      state.prefs.rolled_back_seen = time;
      changed = true;
    }
    if !state.prefs.rolled_back.contains(&key) {
      log::info!("[Setup] the installer rolled back {key}; not offered again until \"Try again\"");
      state.prefs.rolled_back.push(key);
      changed = true;
    }
  }
  if changed {
    state.prefs.save();
  }
}

/// Name of a connected device by `endpoint_key`, None when it is not connected.
fn connected_name(state: &AppState, key: &str) -> Option<String> {
  let (flow, guid) = key.split_once(':')?;
  let endpoints = if flow == "Capture" { &state.capture_endpoints } else { &state.endpoints };
  endpoints
    .iter()
    .find(|e| apo_install::endpoint_guid_of(&e.id).is_some_and(|g| g.eq_ignore_ascii_case(guid)))
    .map(|e| e.name.clone())
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
  item.kind != Kind::Output || default_output_ticked(state.setup.output_anywhere, state.prefs.output_declined)
}

/// Output EQ starts ticked unless the user explicitly unchecked it before and hasn't installed it
/// on any output since. Keeping prefs across a full uninstall keeps both Mic and Output ticked.
fn default_output_ticked(output_anywhere: bool, output_declined: bool) -> bool {
  output_anywhere || !output_declined
}

fn set_ticked(state: &mut AppState, kind: Kind, on: bool) {
  state.setup.choices.retain(|(k, _)| *k != kind);
  state.setup.choices.push((kind, on));
}

#[cfg(test)]
mod tests {
  use super::{default_output_ticked, needs_output_setup};
  use crate::win_audio::apo_install::ApoStatus;

  /// Regression: with the effect on no output at all (declined at setup), echo cancellation on still
  /// needs it on the output in use, so the bar must show.
  #[test]
  fn output_without_the_effect_is_flagged_when_echo_cancellation_needs_it() {
    assert!(needs_output_setup(&ApoStatus::NotOnThisMic, true, false, true));
    assert!(needs_output_setup(&ApoStatus::NotOnThisMic, true, true, false));
    assert!(!needs_output_setup(&ApoStatus::NotOnThisMic, true, false, false));
    assert!(!needs_output_setup(&ApoStatus::Active, true, true, true));
    assert!(!needs_output_setup(&ApoStatus::NotOnThisMic, false, true, true));
  }

  #[test]
  fn output_starts_ticked_unless_explicitly_declined() {
    assert!(default_output_ticked(false, false));
    assert!(!default_output_ticked(false, true));
    assert!(default_output_ticked(true, true));
  }
}
