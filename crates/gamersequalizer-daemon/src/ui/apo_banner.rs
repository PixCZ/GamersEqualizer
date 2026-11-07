//! "The app is not working correctly" info bar under the top bar, on every tab, from app start.
//! Lists each setup problem (mic effect missing / outdated / not on this mic, the output EQ
//! missing on a new output device, the mic in use is not the Windows default) with the reason
//! and a one-click fix. Above it, a warning while an app records into a muted microphone.
//!
//! Fixes that need administrator rights open the setup card (`setup.rs`): it explains why and
//! what will happen, then asks Windows once for all of them. "Not now" keeps the app running
//! degraded; the bar stays until the problem is gone.

use egui::{RichText, Ui};
use std::time::{Duration, Instant};

use super::state::AppState;
use super::theme::{self, Severity};
use crate::health::{Fix, Issue};
use crate::i18n::{tk, tr, trf};
use crate::win_audio::apo_install::{self, ApoCheck, ApoStatus};
use crate::win_audio::WinAudioSystem;

/// Registry + file metadata only: cheap enough to notice a finished install quickly.
const RECHECK_INTERVAL: Duration = Duration::from_secs(3);
/// "Recording into a muted mic" must show up quickly (a few registry reads, one volume read).
const MIC_RECHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct ApoBanner {
  check: Option<ApoCheck>,
  /// Windows default microphone at the last check (full endpoint id).
  default_mic_id: Option<String>,
  checked_at: Option<Instant>,
  /// Result of the last fix click (error or "confirm the administrator prompt").
  message: Option<String>,
  /// An app records while the mic is muted / at 0 % / off (`Issue::MicSilenced`).
  mic_silenced: Option<Issue>,
  mic_checked_at: Option<Instant>,
}

enum Problem {
  Apo { title: String, button: &'static str },
  NewOutput { name: String },
  NotDefaultMic { mic_name: String },
}

/// Re-reads the setup state every few seconds (or right after a fix). Call once per frame.
pub fn refresh(state: &mut AppState, win_audio: &WinAudioSystem) {
  if state.apo_banner.mic_checked_at.is_none_or(|t| t.elapsed() >= MIC_RECHECK_INTERVAL) {
    let default_mic = win_audio.endpoint_lock.get_default_capture_id();
    state.apo_banner.mic_silenced = crate::health::mic_silenced_issue(default_mic);
    state.apo_banner.mic_checked_at = Some(Instant::now());
  }
  let banner = &state.apo_banner;
  if banner.checked_at.is_some_and(|t| t.elapsed() < RECHECK_INTERVAL) {
    return;
  }
  let default_mic = win_audio.endpoint_lock.get_default_capture_id();
  let mic_id = state.selected_mic_endpoint_id.clone().or_else(|| default_mic.clone());
  let check = apo_install::check(mic_id.as_deref());
  let banner = &mut state.apo_banner;
  if check.status == ApoStatus::Active {
    banner.message = None;
  }
  banner.check = Some(check);
  banner.default_mic_id = default_mic;
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
  let id = state.selected_mic_endpoint_id.as_deref().or(state.apo_banner.default_mic_id.as_deref());
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
      }
    }
    if let Some(msg) = state.apo_banner.message.as_deref() {
      ui.label(RichText::new(msg).small());
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

/// True when the selected mic is the Windows default (or nothing is known yet).
pub fn selected_mic_is_default(state: &AppState) -> bool {
  match (state.selected_mic_endpoint_id.as_deref(), state.apo_banner.default_mic_id.as_deref()) {
    (Some(sel), Some(def)) => sel == def,
    _ => true,
  }
}

/// One-line status for the Mic tab, also when everything is fine (the bar then hides).
pub fn render_apo_status_line(ui: &mut Ui, state: &AppState) {
  let Some(check) = state.apo_banner.check.as_ref() else {
    return;
  };
  let version = check.installed_version.as_deref().unwrap_or("?");
  let (text, severity) = match check.status {
    ApoStatus::Active => {
      (trf("Windows mic effect installed and active on this microphone (build {})", &[&version]), Severity::Success)
    }
    ApoStatus::NoMicrophone => (tr("No microphone found").to_string(), Severity::Warning),
    ApoStatus::NotInstalled => {
      (tr("Windows mic effect not installed (see the message at the top)").to_string(), Severity::Warning)
    }
    ApoStatus::Outdated => {
      (trf("Windows mic effect outdated (build {}, see the message at the top)", &[&version]), Severity::Warning)
    }
    ApoStatus::NotOnThisMic => {
      (tr("Windows mic effect not on this microphone (see the message at the top)").to_string(), Severity::Warning)
    }
  };
  theme::status_line(ui, severity, &text);
}

fn problems(state: &AppState) -> Vec<Problem> {
  let mut list = Vec::new();
  if let Some(check) = state.apo_banner.check.as_ref() {
    let apo = match check.status {
      ApoStatus::Active | ApoStatus::NoMicrophone => None,
      ApoStatus::NotInstalled => Some((
        tr(
          "Microphone processing is not installed: Discord, Signal, games ... get your raw microphone, without echo \
            cancellation, noise suppression or EQ.",
        )
        .to_string(),
        tk("Install…"),
      )),
      ApoStatus::Outdated => Some((
        trf(
          "Microphone processing is outdated (installed {}, this app has {}): new fixes and settings may not work.",
          &[&check.installed_version.as_deref().unwrap_or("?"), &check.bundled_version.as_deref().unwrap_or("?")],
        ),
        tk("Update…"),
      )),
      ApoStatus::NotOnThisMic => Some((
        tr("Microphone processing is installed, but not on the microphone in use: apps get it unprocessed.")
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
    ui.label(title);
    if !check.can_install {
      ui.label(
        RichText::new(tr("Installer files were not found next to the app; reinstall the app."))
          .color(theme::palette(ui).warn),
      );
      return;
    }
    // The setup card explains the admin prompt and bundles every admin step into one.
    if !super::setup::visible(state) && ui.add(theme::accent_button(ui, tr(button))).clicked() {
      super::setup::open(state);
    }
  });
}

fn new_output_row(ui: &mut Ui, state: &mut AppState, name: &str) {
  ui.horizontal_wrapped(|ui| {
    ui.label(trf(
      "Output EQ is not set up on \"{}\" (a new sound device?): your headphone correction and EQ are not applied to it.",
      &[&name],
    ));
    if !super::setup::visible(state) && ui.add(theme::accent_button(ui, tr("Set up…"))).clicked() {
      super::setup::open(state);
    }
  });
}

fn default_mic_row(ui: &mut Ui, state: &mut AppState, win_audio: &WinAudioSystem, mic_name: &str) {
  let mut clicked = false;
  ui.horizontal_wrapped(|ui| {
    ui.label(trf(
      "\"{}\" is not the Windows default microphone: Discord, games and other apps use the default one, \
       so they don't get this microphone or its processing.",
      &[&mic_name],
    ));
    clicked = ui
      .add(theme::accent_button(ui, tr("Make it the default")))
      .on_hover_text(tr("Sets it as the Windows default and communications microphone. No admin rights needed."))
      .clicked();
  });
  if clicked {
    if let Some(id) = state.selected_mic_endpoint_id.clone() {
      set_default_mic(state, win_audio, &id);
    }
  }
}
