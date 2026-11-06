//! "The app is not working correctly" info bar under the top bar, on every tab, from app start.
//! Lists each setup problem (mic effect missing / outdated / not on this mic, the mic in use is
//! not the Windows default) with the reason and a one-click fix.
//!
//! Like Android's permission rationale: a fix that triggers a system prompt (UAC) first says
//! why it is needed and what will happen, and only then asks Windows. "Not now" keeps the app
//! running degraded; the bar stays until the problem is gone.

use egui::{RichText, Ui};
use std::time::{Duration, Instant};

use super::state::AppState;
use super::theme::{self, Severity};
use crate::i18n::{tk, tr, trf};
use crate::win_audio::apo_install::{self, ApoCheck, ApoStatus};
use crate::win_audio::WinAudioSystem;

/// Registry + file metadata only: cheap enough to notice a finished install quickly.
const RECHECK_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Default)]
pub struct ApoBanner {
  check: Option<ApoCheck>,
  /// Windows default microphone at the last check (full endpoint id).
  default_mic_id: Option<String>,
  checked_at: Option<Instant>,
  /// Result of the last fix click (error or "confirm the administrator prompt").
  message: Option<String>,
  /// The "why administrator rights" explanation is open (step before the UAC prompt).
  explaining_install: bool,
}

enum Problem {
  Apo { title: String, button: &'static str },
  NotDefaultMic { mic_name: String },
}

/// Re-reads the setup state every few seconds (or right after a fix). Call once per frame.
pub fn refresh(state: &mut AppState, win_audio: &WinAudioSystem) {
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
    banner.explaining_install = false;
  }
  banner.check = Some(check);
  banner.default_mic_id = default_mic;
  banner.checked_at = Some(Instant::now());
}

pub fn has_problems(state: &AppState) -> bool {
  !problems(state).is_empty()
}

/// Full-width error info bar; render only when `has_problems`.
pub fn render_health_bar(ui: &mut Ui, state: &mut AppState, win_audio: &WinAudioSystem) {
  let list = problems(state);
  theme::info_bar(ui, Severity::Error, |ui| {
    ui.label(theme::strong(tr("GamersEqualizer is not working correctly"), 15.0));
    for problem in list {
      match problem {
        Problem::Apo { title, button } => apo_row(ui, state, &title, button),
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
    if !state.apo_banner.explaining_install && ui.add(theme::accent_button(ui, tr(button))).clicked() {
      state.apo_banner.explaining_install = true;
    }
  });
  if !state.apo_banner.explaining_install || !check.can_install {
    return;
  }
  // Rationale before the system prompt: what, why admin, what the user will notice.
  theme::inset_frame(ui).show(ui, |ui| {
    ui.set_width(ui.available_width());
    ui.label(theme::strong(tr("Why does this need administrator rights?"), 14.0));
    ui.label(tr(
      "The processing runs inside Windows audio itself (a microphone effect), so every app gets the clean voice \
       without any setup in that app. Installing a Windows audio effect is a system change, so Windows will ask \
       you to allow it.\n\
       What happens: a Windows prompt asks for permission, a small installer window runs, and Windows audio \
       restarts for a few seconds. Apps that use the microphone (Discord, games) may need to be reopened.\n\
       Nothing else is changed, and the app keeps working if you say no (just without the processing).",
    ));
    ui.horizontal(|ui| {
      if ui.add(theme::accent_button(ui, tr("Continue – ask Windows"))).clicked() {
        state.apo_banner.explaining_install = false;
        if let Some(guid) = check.endpoint_guid.as_deref() {
          state.apo_banner.message = Some(match apo_install::launch_install(guid) {
            Ok(()) => {
              tr("Installer started: confirm the Windows prompt. This message disappears when it is done.").to_string()
            }
            Err(e) => {
              log::warn!("[APO] install could not start: {e}");
              e
            }
          });
        }
        state.apo_banner.checked_at = None;
      }
      if ui.button(tr("Not now")).clicked() {
        state.apo_banner.explaining_install = false;
      }
    });
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
