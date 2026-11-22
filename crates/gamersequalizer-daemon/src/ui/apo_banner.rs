//! "The app is not working correctly" info bar under the top bar, on every tab, from app start.
//! Lists each setup problem (mic effect missing / outdated / not on this mic, the output EQ
//! missing on a new output device, the Windows default output moved away, the mic in use is not the Windows default) with the reason
//! and a one-click fix. Above it, a warning while an app records into a muted microphone.
//!
//! Fixes that need administrator rights open the setup card (`setup.rs`): it explains why and
//! what will happen, then asks Windows once for all of them. "Not now" keeps the app running
//! degraded; the bar stays until the problem is gone.

use egui::{RichText, Ui};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use super::state::AppState;
use super::theme::{self, Severity};
use crate::health::{Fix, Issue, OutputChange, RunStatus};
use crate::i18n::{tk, tr, trf};
use crate::win_audio::WinAudioSystem;
use crate::win_audio::apo_install::{self, ApoCheck, ApoStatus};
use gamersequalizer_common::shm::MIC_CHAIN_OTHERS;

/// Registry + file metadata only: cheap enough to notice a finished install quickly.
const RECHECK_INTERVAL: Duration = Duration::from_secs(3);
/// "Recording into a muted mic" must show up quickly (a few registry reads, one volume read).
const MIC_RECHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct ApoBanner {
  check: Option<ApoCheck>,
  /// The microphone in use at the last check (`ChatMixController::mic_device_id`): the one the
  /// processing is checked on.
  mic_id: Option<String>,
  /// Windows default microphone at the last check (full endpoint id).
  default_mic_id: Option<String>,
  checked_at: Option<Instant>,
  /// Result of the last fix click (error or "confirm the administrator prompt").
  message: Option<String>,
  /// An app records while the mic is muted / at 0 % / off (`Issue::MicSilenced`).
  mic_silenced: Option<Issue>,
  mic_checked_at: Option<Instant>,
  /// The service's verdict on our effects really running (`run_status.json`) and the file's
  /// modification time when read.
  run: RunStatus,
  run_mtime: Option<SystemTime>,
}

enum Problem {
  Apo {
    title: String,
    button: &'static str,
  },
  NewOutput {
    name: String,
  },
  NotDefaultMic {
    mic_name: String,
  },
  /// The Windows default output is not the one the user had: what the tray's badge says too.
  OutputChanged(OutputChange),
}

/// Re-reads the setup state every few seconds (or right after a fix). Call once per frame.
pub fn refresh(state: &mut AppState, win_audio: &WinAudioSystem) {
  if state.apo_banner.mic_checked_at.is_none_or(|t| t.elapsed() >= MIC_RECHECK_INTERVAL) {
    state.apo_banner.mic_silenced = crate::health::mic_silenced_issue(win_audio.connected_mic());
    state.apo_banner.mic_checked_at = Some(Instant::now());
    // The service rewrites it only when its verdict changes.
    let mtime = std::fs::metadata(RunStatus::path(state.storage.base_dir())).and_then(|m| m.modified()).ok();
    if mtime != state.apo_banner.run_mtime {
      state.apo_banner.run = RunStatus::load(state.storage.base_dir());
      state.apo_banner.run_mtime = mtime;
    }
  }
  let banner = &state.apo_banner;
  if banner.checked_at.is_some_and(|t| t.elapsed() < RECHECK_INTERVAL) {
    return;
  }
  // The same microphone as the mixer, the service's health check and sidetone.
  let mic_id = win_audio.chatmix.mic_device_id.clone();
  let check = apo_install::check(mic_id.as_deref());
  let banner = &mut state.apo_banner;
  if check.status == ApoStatus::Active {
    banner.message = None;
  }
  banner.check = Some(check);
  banner.mic_id = mic_id;
  banner.default_mic_id = win_audio.endpoint_lock.get_default_capture_id();
  banner.checked_at = Some(Instant::now());
}

pub fn has_problems(state: &AppState) -> bool {
  !problems(state).is_empty()
}

/// Mic processing state from the last check (None before the first one).
pub fn mic_check(state: &AppState) -> Option<&ApoCheck> {
  state.apo_banner.check.as_ref()
}

/// Display name of the microphone the processing is checked on.
pub fn mic_in_use_name(state: &AppState) -> String {
  let id = state.apo_banner.mic_id.as_deref();
  state
    .capture_endpoints
    .iter()
    .find(|e| Some(e.id.as_str()) == id)
    .map(|e| e.name.clone())
    .unwrap_or_else(|| tr("The selected microphone").to_string())
}

/// Check again on the next frame (an installer was started).
pub fn recheck(state: &mut AppState) {
  state.apo_banner.checked_at = None;
}

pub fn has_mic_warning(state: &AppState) -> bool {
  state.apo_banner.mic_silenced.is_some()
}

/// Warning bar while an app records into a muted / silent / missing microphone; render only
/// when `has_mic_warning`. The background service shows the same as a tray badge + notification.
pub fn render_mic_warning(ui: &mut Ui, state: &mut AppState) {
  let Some(issue) = state.apo_banner.mic_silenced.clone() else {
    return;
  };
  let mut clicked = None;
  theme::info_bar(ui, Severity::Warning, |ui| {
    ui.label(theme::strong(issue.title(), 15.0));
    ui.horizontal_wrapped(|ui| {
      ui.label(tr("Nobody hears you."));
      for (label, fix) in issue.fixes() {
        if ui.add(theme::accent_button(ui, &label)).clicked() {
          clicked = Some(fix);
        }
      }
    });
  });
  ui.add_space(6.0);
  match clicked {
    Some(Fix::UnmuteMic(id)) => crate::win_audio::chatmix::set_endpoint_mute(&id, false),
    Some(Fix::RaiseMicVolume(id)) => crate::win_audio::chatmix::set_endpoint_volume(&id, 1.0),
    Some(Fix::OpenSoundSettings) => crate::win_audio::shell::open_sound_settings(),
    _ => return,
  }
  state.apo_banner.mic_checked_at = None;
}

/// Our effect in Windows is not doing its job: not installed or not on the microphone in use, or
/// the service saw it process nothing while apps recorded or sound played.
pub fn effect_not_running(state: &AppState) -> bool {
  let missing = state
    .apo_banner
    .check
    .as_ref()
    .is_some_and(|c| matches!(c.status, ApoStatus::NotInstalled | ApoStatus::NotOnThisMic));
  let silent = run_status(state).is_some_and(|r| r.mic_not_running.is_some() || r.output_not_running.is_some());
  missing || silent
}

/// Full-width error info bar; render only when `has_problems`.
pub fn render_health_bar(ui: &mut Ui, state: &mut AppState, win_audio: &WinAudioSystem) {
  let list = problems(state);
  theme::info_bar(ui, Severity::Error, |ui| {
    ui.label(theme::strong(tr("GamersEqualizer is not working correctly"), 15.0));
    for problem in list {
      match problem {
        Problem::Apo { title, button } => apo_row(ui, state, &title, button),
        Problem::NewOutput { name } => new_output_row(ui, state, &name),
        Problem::NotDefaultMic { mic_name } => default_mic_row(ui, state, win_audio, &mic_name),
        Problem::OutputChanged(change) => output_changed_row(ui, state, win_audio, &change),
      }
    }
    if let Some(msg) = state.apo_banner.message.as_deref() {
      ui.label(theme::caption(msg));
    }
  });
  ui.add_space(6.0);
}

/// Makes `mic_id` the Windows default for all roles (so Discord & co. use it) and re-checks.
pub fn set_default_mic(state: &mut AppState, win_audio: &WinAudioSystem, mic_id: &str) {
  let banner = &mut state.apo_banner;
  match win_audio.endpoint_lock.set_as_default_recording_device(mic_id) {
    Ok(()) => {
      banner.default_mic_id = Some(mic_id.to_string());
      banner.message = None;
    }
    Err(e) => {
      log::warn!("[Setup] setting the Windows default microphone failed: {e}");
      banner.message = Some(trf("Windows refused to change the default microphone: {}", &[&e]));
    }
  }
  banner.checked_at = None;
}

/// The service's verdict on our effects really running; None while the service is not running
/// (the file then describes an old run).
pub fn run_status(state: &AppState) -> Option<&RunStatus> {
  let service = state.shm_header().is_some_and(|h| h.daemon_active.load(Ordering::Relaxed));
  service.then_some(&state.apo_banner.run)
}

/// Newest note from inside audiodg about `prefix` ("Mic chain", "Output EQ"; any case) and one
/// endpoint (`{guid}`, None = any): a note tagged with another endpoint's GUID
/// (effects on several devices) is not this device's reason. Untagged notes still count.
pub fn newest_note_on<'a>(run: &'a RunStatus, prefix: &str, guid: Option<&str>) -> Option<&'a str> {
  let prefix = prefix.to_lowercase();
  let tag = guid.map(|g| format!("[{}]", g.to_lowercase()));
  // Not the installer's own lines ("installer: Target: ... Mic Chain (EFX)"): same source.
  let about = |note: &&String| {
    let note = note.to_lowercase();
    let other_device = tag.as_ref().is_some_and(|tag| note.contains("[{") && !note.contains(tag.as_str()));
    note.contains(&prefix) && !note.contains(" installer: ") && !other_device
  };
  let notes = &run.apo_notes;
  // Sound settings also create our effects, only to list them: what audiodg did comes first.
  let from_audiodg = notes.iter().filter(about).find(|note| note.to_lowercase().contains("audiodg"));
  from_audiodg.or_else(|| notes.iter().find(about)).map(String::as_str)
}

/// True when the selected mic is the Windows default (or nothing is known yet).
pub fn selected_mic_is_default(state: &AppState) -> bool {
  match (state.selected_mic_endpoint_id.as_deref(), state.apo_banner.default_mic_id.as_deref()) {
    (Some(sel), Some(def)) => sel == def,
    _ => true,
  }
}

/// One-line status for the Mic tab, also when everything is fine (the bar then hides).
/// "Installed" comes from the registry; "running" only from the effect's own telemetry.
pub fn render_apo_status_line(ui: &mut Ui, state: &mut AppState) {
  let live = state.mic_dsp_live();
  let several =
    live && state.shm_header().is_some_and(|h| h.mic_chain_flags.load(Ordering::Relaxed) & MIC_CHAIN_OTHERS != 0);
  let run = run_status(state);
  let not_running = run.is_some_and(|r| r.mic_not_running.is_some());
  let guid = state.apo_banner.check.as_ref().and_then(|c| c.endpoint_guid.clone());
  let note = run.and_then(|r| newest_note_on(r, "Mic chain", guid.as_deref())).map(str::to_string);
  let Some(check) = state.apo_banner.check.as_ref() else {
    return;
  };
  let version = check.installed_version.as_deref().unwrap_or("?");
  let (text, severity) = match check.status {
    // The service saw apps record from this mic while our effect processed nothing. "Does not
    // work", not "Windows does not run it": the APO may run and only pass audio through.
    ApoStatus::Active if not_running => (
      trf("The mic effect does NOT work on this microphone (build {}): apps get the raw microphone", &[&version]),
      Severity::Error,
    ),
    ApoStatus::Active if live => {
      (trf("Windows mic effect installed and running on this microphone (build {})", &[&version]), Severity::Success)
    }
    ApoStatus::Active => (
      trf(
        "Windows mic effect installed on this microphone (build {}); it runs while an app uses the microphone",
        &[&version],
      ),
      Severity::Success,
    ),
    ApoStatus::NoMicrophone => (tr("No microphone found").to_string(), Severity::Warning),
    ApoStatus::NotInstalled => {
      (tr("Windows mic effect not installed (see the message at the top)").to_string(), Severity::Warning)
    }
    ApoStatus::Outdated => {
      (trf("Sound effects need an update (build {}, see the message at the top)", &[&version]), Severity::Warning)
    }
    ApoStatus::NotOnThisMic => {
      (tr("Windows mic effect not on this microphone (see the message at the top)").to_string(), Severity::Warning)
    }
  };
  let switch_on = check.endpoint_guid.clone().filter(|_| not_running && check.can_install);
  theme::status_line(ui, severity, &text);
  if let Some(note) = note.filter(|_| not_running) {
    theme::status_line(ui, Severity::Info, &trf("Last report from Windows audio: {}", &[&note]));
  }
  // Apps recording in different processing modes (MFX) or from different microphones each get
  // their own chain; the APO publishes the values of one of them, the first while it runs.
  if several {
    let text = match crate::mic_usage::longest_recording_app() {
      Some(app) => trf(
        "Several recordings are processed at once, each separately (apps record in different modes or from \
         different microphones). The values shown here belong to the one that started first: {}",
        &[&app],
      ),
      None => tr(
        "Several recordings are processed at once, each separately (apps record in different modes or from \
         different microphones). The values shown here belong to the one that started first.",
      )
      .to_string(),
    };
    theme::status_line(ui, Severity::Info, &text);
  }
  let Some(guid) = switch_on else {
    return;
  };
  let clicked = ui
    .add(theme::accent_button(ui, tr("Try another effect slot (administrator)...")))
    .on_hover_text(tr(
      "Moves the mic effect between the two places where Windows can run it (device effects and mode effects). \
       Some microphones run only one of them. Windows asks for permission and audio restarts for a few seconds.",
    ))
    .clicked();
  if clicked {
    state.apo_banner.message =
      apo_install::launch_switch_mic_slot(&guid).err().map(|e| trf("The installer did not start: {}", &[&e]));
    recheck(state);
  }
  if let Some(msg) = state.apo_banner.message.as_deref() {
    ui.label(theme::caption(msg));
  }
}

fn problems(state: &AppState) -> Vec<Problem> {
  let mut list = Vec::new();
  if let Some(change) = run_status(state).and_then(|r| r.output_changed.clone()) {
    list.push(Problem::OutputChanged(change));
  }
  if let Some(check) = state.apo_banner.check.as_ref() {
    let apo = match check.status {
      ApoStatus::Active | ApoStatus::NoMicrophone => None,
      ApoStatus::NotInstalled => Some((
        tr(
          "Microphone processing is not installed: apps get your raw microphone (no echo cancellation, noise \
           suppression or EQ).",
        )
        .to_string(),
        tk("Install…"),
      )),
      ApoStatus::Outdated => Some((
        tr("Sound effects need an update. Newer features do not work until you update them.").to_string(),
        tk("Update…"),
      )),
      ApoStatus::NotOnThisMic => Some((
        tr(
          "Microphone processing is not on the microphone in use: apps get it raw (no echo cancellation, noise \
           suppression or EQ).",
        )
        .to_string(),
        tk("Enable on this microphone…"),
      )),
    };
    if let Some((title, button)) = apo {
      list.push(Problem::Apo { title, button });
    }
  }
  if let Some(name) = super::setup::output_needs_setup(state) {
    list.push(Problem::NewOutput { name: name.to_string() });
  }
  if !selected_mic_is_default(state) {
    let sel = state.selected_mic_endpoint_id.as_deref();
    let mic_name = state
      .capture_endpoints
      .iter()
      .find(|e| Some(e.id.as_str()) == sel)
      .map(|e| e.name.clone())
      .unwrap_or_else(|| tr("The selected microphone").to_string());
    list.push(Problem::NotDefaultMic { mic_name });
  }
  list
}

fn apo_row(ui: &mut Ui, state: &mut AppState, title: &str, button: &str) {
  let Some(check) = state.apo_banner.check.clone() else {
    return;
  };
  ui.horizontal_wrapped(|ui| {
    // The button first (as in every row): a long line wraps and would push it out of sight.
    if !check.can_install {
      ui.label(title);
      ui.label(
        RichText::new(tr("Installer files were not found next to the app; reinstall the app."))
          .color(theme::palette(ui).warn),
      );
      return;
    }
    // The setup card explains the admin prompt and bundles every admin step into one.
    if !super::setup::visible(state) && setup_button(ui, tr(button)).clicked() {
      super::setup::open(state);
    }
    ui.label(title);
  });
}

/// The accent button that opens the setup card, with what it costs said on hover.
fn setup_button(ui: &mut Ui, text: &str) -> egui::Response {
  ui.add(theme::accent_button(ui, text))
    .on_hover_text(tr("Opens the setup: one administrator prompt, then Windows audio restarts for a few seconds."))
}

fn new_output_row(ui: &mut Ui, state: &mut AppState, name: &str) {
  ui.horizontal_wrapped(|ui| {
    // The button first: a long line wraps and would push it out of sight.
    if !super::setup::visible(state) && setup_button(ui, tr("Set up…")).clicked() {
      super::setup::open(state);
    }
    ui.label(trf("Echo cancellation and EQ do not work on \"{}\"", &[&name]));
  });
}

fn output_changed_row(ui: &mut Ui, state: &mut AppState, win_audio: &WinAudioSystem, change: &OutputChange) {
  let mut keep = None;
  // What happened on its own line, the choices below it: both device names are long, and in one
  // row the text and the two buttons ran into each other.
  ui.label(trf("Windows default output changed to \"{}\"", &[&change.current_name]));
  ui.horizontal_wrapped(|ui| {
    // The same two choices as in the tray menu (`Issue::fixes`).
    if ui.add(theme::accent_button(ui, &trf("Switch back to \"{}\"", &[&change.expected_name]))).clicked() {
      keep = Some(false);
    }
    if ui.button(trf("Keep \"{}\"", &[&change.current_name])).clicked() {
      keep = Some(true);
    }
  });
  let Some(keep) = keep else {
    return;
  };
  if keep {
    crate::health::remember_output(state.storage.base_dir(), &change.current_id);
  } else if let Err(e) = win_audio.endpoint_lock.set_as_default_device(&change.expected_id) {
    log::warn!("[Setup] switching the default output back failed: {e}");
    state.apo_banner.message = Some(trf("Windows refused to change the default output: {}", &[&e.to_string()]));
    return;
  }
  // The service rewrites its verdict within a few seconds; the row goes now.
  state.apo_banner.run.output_changed = None;
}

fn default_mic_row(ui: &mut Ui, state: &mut AppState, win_audio: &WinAudioSystem, mic_name: &str) {
  let mut clicked = false;
  ui.horizontal_wrapped(|ui| {
    clicked = ui
      .add(theme::accent_button(ui, tr("Make it the default")))
      .on_hover_text(tr("Sets it as the Windows default and communications microphone. No admin rights needed."))
      .clicked();
    ui.label(trf(
      "\"{}\" is not the Windows default microphone: Discord and games use the default one, not this one.",
      &[&mic_name],
    ));
  });
  if clicked && let Some(id) = state.selected_mic_endpoint_id.clone() {
    set_default_mic(state, win_audio, &id);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn newest_note_prefers_what_audiodg_did() {
    let settings = "2026-11-11T07:30:00Z SystemSettings.exe pid 9: GamersEqualizer Mic Chain: class requested";
    let audiodg = "2026-11-11T07:25:00Z audiodg.exe pid 6836: Mic chain: stream stopped after 0 blocks, 0 processed";
    let run = RunStatus { apo_notes: vec![settings.to_string(), audiodg.to_string()], ..Default::default() };
    assert_eq!(newest_note_on(&run, "Mic chain", None), Some(audiodg));
    let run = RunStatus { apo_notes: vec![settings.to_string()], ..Default::default() };
    assert_eq!(newest_note_on(&run, "mic chain", None), Some(settings));
    assert_eq!(newest_note_on(&run, "Output EQ", None), None);
    let installer = "2026-11-11T12:26:29Z installer: Target: Microphone Array: GamersEqualizer Mic Chain (MFX)";
    let run = RunStatus { apo_notes: vec![installer.to_string()], ..Default::default() };
    assert_eq!(
      newest_note_on(&run, "Mic chain", None),
      None,
      "what the installer printed is not a report from audiodg"
    );

    let mine = "2026-11-11T07:20:00Z audiodg.exe pid 6: Mic chain [{aa-01}]: stream stopped after 0 blocks";
    let other = "2026-11-11T07:21:00Z audiodg.exe pid 6: Mic chain [{bb-02}]: stream stopped after 9 blocks";
    let run = RunStatus { apo_notes: vec![other.to_string(), mine.to_string()], ..Default::default() };
    assert_eq!(newest_note_on(&run, "Mic chain", Some("{AA-01}")), Some(mine), "another device's note is skipped");
    assert_eq!(newest_note_on(&run, "Mic chain", None), Some(other));
    let run = RunStatus { apo_notes: vec![audiodg.to_string()], ..Default::default() };
    assert_eq!(newest_note_on(&run, "Mic chain", Some("{aa-01}")), Some(audiodg), "untagged notes still count");
  }
}
