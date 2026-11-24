//! The problems the checks report: their titles, details and the fixes offered for each.

use super::*;

/// Recording apps named in a title; more become "+N".
const MAX_APPS_IN_TITLE: usize = 2;

/// The default output moved away from the expected one; also in `run_status.json`, for the window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputChange {
  pub expected_id: String,
  pub expected_name: String,
  pub current_id: String,
  pub current_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Issue {
  OutputChanged(OutputChange),
  /// `format`: name of the format that was on, if known.
  SpatialOff {
    device_id: String,
    device_name: String,
    format: Option<String>,
  },
  SpatialChanged {
    device_id: String,
    device_name: String,
    expected: SpatialFormat,
    current: SpatialFormat,
  },
  MicProcessing {
    status: ApoStatus,
    key: String,
  },
  NotDefaultMic {
    mic_id: String,
    mic_name: String,
  },
  OutputProcessing {
    device_name: String,
    key: String,
  },
  /// `apps` already joined for display.
  MicSilenced {
    why: Silenced,
    apps: String,
    mic_id: Option<String>,
  },
  /// Our effect is listed on the device but did not run while it was in use. `key` ignores it.
  NotRunning {
    flow: Flow,
    device_id: String,
    device_name: String,
    key: String,
  },
}

/// What a menu item under an issue does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fix {
  SwitchOutput(String),
  KeepOutput(String),
  OpenSoundSettings,
  /// Settings page of the default output (spatial sound choice).
  OpenSpatialSettings,
  IgnoreSpatial(String),
  /// Accept `.1` as the user's format on output `.0`.
  KeepSpatial(String, SpatialFormat),
  MakeMicDefault(String),
  /// Move the mic effect on this microphone (endpoint id) to its other slot (admin prompt).
  SwitchMicSlot(String),
  UnmuteMic(String),
  RaiseMicVolume(String),
  /// The window's banner explains the admin prompt before installing.
  OpenWindow,
  /// Install the bundled effects again where ours already are (admin prompt, audio restart).
  UpdateEffects,
  Ignore(String),
}

impl Fix {
  /// Only shows a window (ours, a Windows settings page) and needs nothing from the monitor
  /// (`show_window` runs it).
  pub fn shows_window(&self) -> bool {
    matches!(self, Fix::OpenSoundSettings | Fix::OpenSpatialSettings | Fix::OpenWindow)
  }

  /// Starts an elevated script: Windows shows its UAC prompt in front only while this process
  /// has the foreground.
  pub fn asks_admin(&self) -> bool {
    matches!(self, Fix::UpdateEffects | Fix::SwitchMicSlot(_))
  }
}

impl Issue {
  /// One line for the menu and tooltip.
  pub fn title(&self) -> String {
    match self {
      Issue::OutputChanged(OutputChange { current_name, .. }) => {
        trf("Windows default output changed to \"{}\"", &[current_name])
      }
      Issue::SpatialOff { device_name, format: Some(format), .. } if !format.is_empty() => {
        trf("Spatial sound ({}) turned off on \"{}\"", &[format, device_name])
      }
      Issue::SpatialOff { device_name, .. } => trf("Spatial sound turned off on \"{}\"", &[device_name]),
      Issue::SpatialChanged { device_name, expected, current, .. } => trf(
        "Spatial sound on \"{}\" is {} instead of {}",
        &[device_name, &current.label(tr("another format")), &expected.label(tr("your format"))],
      ),
      Issue::MicProcessing { status: ApoStatus::Outdated, .. } => tr("Sound effects need an update").to_string(),
      Issue::MicProcessing { status: ApoStatus::NotOnThisMic, .. } => {
        tr("Microphone processing is not set up on the active microphone").to_string()
      }
      Issue::MicProcessing { .. } => tr("Microphone processing is not installed").to_string(),
      Issue::NotDefaultMic { mic_name, .. } => trf("\"{}\" is not the Windows default microphone", &[mic_name]),
      Issue::OutputProcessing { device_name, .. } => {
        trf("Echo cancellation and EQ do not work on \"{}\"", &[device_name])
      }
      Issue::MicSilenced { why: Silenced::Muted, apps, .. } => {
        trf("{} is recording, but the microphone is muted", &[apps])
      }
      Issue::MicSilenced { why: Silenced::ZeroVolume, apps, .. } => {
        trf("{} is recording, but the microphone volume is 0 %", &[apps])
      }
      Issue::MicSilenced { why: Silenced::NoMicrophone, apps, .. } => {
        trf("{} is recording, but no microphone is turned on", &[apps])
      }
      Issue::NotRunning { flow: Flow::Capture, device_name, .. } => {
        trf("Microphone processing does not work on \"{}\"", &[device_name])
      }
      Issue::NotRunning { flow: Flow::Render, device_name, .. } => {
        trf("Echo cancellation and EQ do not work on \"{}\"", &[device_name])
      }
    }
  }

  /// Second line of the Windows notification: what it means for the user.
  pub fn detail(&self) -> &'static str {
    match self {
      Issue::OutputChanged(_) => tr("Audio is now playing on another device. Use the tray menu to switch it back."),
      Issue::SpatialOff { .. } => tr("Turn it back on in Windows sound settings (accessible from the tray menu)."),
      Issue::SpatialChanged { .. } => {
        tr("Switch it back in Windows sound settings or keep the new one — both options are in the tray menu.")
      }
      Issue::MicProcessing { status: ApoStatus::Outdated, .. } => {
        tr("Newer features do not work until you update them.")
      }
      Issue::MicProcessing { .. } => {
        tr("Apps are receiving your raw microphone without echo cancellation, noise suppression, or EQ.")
      }
      Issue::NotDefaultMic { .. } => tr("Discord, games and other apps use the default microphone, not this one."),
      Issue::OutputProcessing { .. } => tr(
        "Connected a new audio device? Open GamersEqualizer to set it up for headphone correction, EQ, and echo cancellation.",
      ),
      Issue::MicSilenced { .. } => {
        tr("No one can hear you. Fix this from the GamersEqualizer icon in the notification area.")
      }
      // Only what was seen: it processed nothing. Why is in the log ([ApoNote], [ApoLink]).
      Issue::NotRunning { flow: Flow::Capture, .. } => tr(
        "Microphone processing is installed, but it didn't process anything during recording — apps receive the unprocessed microphone. Check the app log for details.",
      ),
      Issue::NotRunning { flow: Flow::Render, .. } => tr(
        "The output effect is installed, but it didn't process anything during playback — EQ and echo cancellation are inactive. Check the app log for details.",
      ),
    }
  }

  /// One notification per problem and cooldown, whatever names it carries.
  pub(super) fn notify_key(&self) -> String {
    match self {
      Issue::OutputChanged(OutputChange { current_id, .. }) => format!("output:{current_id}"),
      Issue::SpatialOff { device_id, .. } => format!("spatial:{device_id}"),
      Issue::SpatialChanged { device_id, current, .. } => format!("spatial_format:{device_id}:{}", current.id),
      Issue::MicProcessing { key, .. } | Issue::OutputProcessing { key, .. } | Issue::NotRunning { key, .. } => {
        key.clone()
      }
      Issue::NotDefaultMic { mic_id, .. } => not_default_mic_key(mic_id),
      Issue::MicSilenced { .. } => "mic_silenced".to_string(),
    }
  }

  /// Menu items under the title: (label, action), the fix first.
  pub fn fixes(&self) -> Vec<(String, Fix)> {
    match self {
      Issue::OutputChanged(OutputChange { expected_id, expected_name, current_id, current_name }) => vec![
        (trf("Switch back to \"{}\"", &[expected_name]), Fix::SwitchOutput(expected_id.clone())),
        (trf("Keep \"{}\"", &[current_name]), Fix::KeepOutput(current_id.clone())),
      ],
      Issue::SpatialOff { device_id, .. } => vec![
        (tr("Open spatial sound settings...").to_string(), Fix::OpenSpatialSettings),
        (tr("Ignore").to_string(), Fix::IgnoreSpatial(device_id.clone())),
      ],
      Issue::SpatialChanged { device_id, current, .. } => vec![
        (tr("Open spatial sound settings...").to_string(), Fix::OpenSpatialSettings),
        (
          trf("Keep \"{}\"", &[&current.label(tr("the new format"))]),
          Fix::KeepSpatial(device_id.clone(), current.clone()),
        ),
      ],
      Issue::MicProcessing { status: ApoStatus::Outdated, key } => vec![
        (tr("Update sound effects (administrator, restarts Windows audio)...").to_string(), Fix::UpdateEffects),
        (tr("Ignore").to_string(), Fix::Ignore(key.clone())),
      ],
      Issue::MicProcessing { key, .. } => vec![
        (tr("Fix in GamersEqualizer...").to_string(), Fix::OpenWindow),
        (tr("Ignore").to_string(), Fix::Ignore(key.clone())),
      ],
      Issue::NotDefaultMic { mic_id, .. } => vec![
        (tr("Make it the default").to_string(), Fix::MakeMicDefault(mic_id.clone())),
        (tr("Ignore").to_string(), Fix::Ignore(not_default_mic_key(mic_id))),
      ],
      Issue::OutputProcessing { key, .. } => vec![
        (tr("Set up in GamersEqualizer...").to_string(), Fix::OpenWindow),
        (tr("Ignore").to_string(), Fix::Ignore(key.clone())),
      ],
      // Some mics never start an endpoint effect (EFX) but run mode effects (MFX), or the
      // other way round: the other slot is the fix that works without knowing which.
      Issue::NotRunning { flow: Flow::Capture, device_id, key, .. } => vec![
        (tr("Try another effect slot (administrator)...").to_string(), Fix::SwitchMicSlot(device_id.clone())),
        (tr("Open sound settings...").to_string(), Fix::OpenSoundSettings),
        (tr("Ignore").to_string(), Fix::Ignore(key.clone())),
      ],
      // The audio enhancements switch is in the device's sound settings.
      Issue::NotRunning { key, .. } => vec![
        (tr("Open sound settings...").to_string(), Fix::OpenSoundSettings),
        (tr("Ignore").to_string(), Fix::Ignore(key.clone())),
      ],
      // Transient: no "Ignore"; it goes away when the recording stops.
      Issue::MicSilenced { why, mic_id, .. } => match (why, mic_id) {
        (Silenced::Muted, Some(id)) => vec![(tr("Unmute the microphone").to_string(), Fix::UnmuteMic(id.clone()))],
        (Silenced::ZeroVolume, Some(id)) => {
          vec![(tr("Set the microphone volume to 100 %").to_string(), Fix::RaiseMicVolume(id.clone()))]
        }
        _ => vec![(tr("Open sound settings...").to_string(), Fix::OpenSoundSettings)],
      },
    }
  }

  /// A few words for the tray flyout, which has one line per issue; `title` goes in its tooltip.
  pub fn short_title(&self) -> &'static str {
    match self {
      Issue::OutputChanged(_) => tr("Default output changed"),
      Issue::SpatialOff { .. } => tr("Spatial sound is off"),
      Issue::SpatialChanged { .. } => tr("Spatial sound changed"),
      // One DLL runs the mic chain, the output EQ and the stream EQ: not "microphone".
      Issue::MicProcessing { status: ApoStatus::Outdated, .. } => tr("Sound effects need an update"),
      Issue::MicProcessing { status: ApoStatus::NotOnThisMic, .. } => tr("Mic processing is on another mic"),
      Issue::MicProcessing { .. } => tr("Microphone processing is not installed"),
      Issue::NotDefaultMic { .. } => tr("Mic is not the default"),
      Issue::OutputProcessing { .. } => tr("Output is not set up"),
      Issue::MicSilenced { why: Silenced::Muted, .. } => tr("Microphone is muted"),
      Issue::MicSilenced { why: Silenced::ZeroVolume, .. } => tr("Microphone volume is 0 %"),
      Issue::MicSilenced { why: Silenced::NoMicrophone, .. } => tr("No microphone is on"),
      Issue::NotRunning { flow: Flow::Capture, .. } => tr("Mic processing does not run"),
      Issue::NotRunning { flow: Flow::Render, .. } => tr("Output effect does not run"),
    }
  }

  /// The first of `fixes` with a button-sized label: (short label, full menu label, fix).
  pub fn first_fix(&self) -> Option<(&'static str, String, Fix)> {
    let (label, fix) = self.fixes().into_iter().next()?;
    let short = match &fix {
      Fix::SwitchOutput(_) => tr("Switch back"),
      Fix::KeepOutput(_) | Fix::KeepSpatial(..) => tr("Keep"),
      Fix::OpenSoundSettings | Fix::OpenSpatialSettings => tr("Settings"),
      Fix::IgnoreSpatial(_) | Fix::Ignore(_) => tr("Ignore"),
      Fix::MakeMicDefault(_) => tr("Make default"),
      Fix::SwitchMicSlot(_) => tr("Other slot"),
      Fix::UnmuteMic(_) => tr("Unmute"),
      Fix::RaiseMicVolume(_) => tr("Set 100 %"),
      Fix::UpdateEffects => tr("Update"),
      Fix::OpenWindow => match self {
        Issue::MicProcessing { status: ApoStatus::NotOnThisMic, .. } => tr("Fix"),
        _ => tr("Set up"),
      },
    };
    Some((short, label, fix))
  }
}

/// `mic`: the microphone in use while it is connected (`WinAudioSystem::connected_mic`).
pub fn mic_silenced_issue(mic: Option<String>) -> Option<Issue> {
  let (apps, why) = mic_usage::check_silenced(mic.as_deref())?;
  Some(Issue::MicSilenced { why, apps: join_apps(&apps), mic_id: mic })
}

/// "Discord.exe", "Discord.exe, obs64.exe", "Discord.exe, obs64.exe +2".
pub fn join_apps(apps: &[String]) -> String {
  let shown: Vec<&str> = apps.iter().take(MAX_APPS_IN_TITLE).map(String::as_str).collect();
  let rest = apps.len().saturating_sub(MAX_APPS_IN_TITLE);
  if rest > 0 { format!("{} +{rest}", shown.join(", ")) } else { shown.join(", ") }
}

pub(super) fn not_default_mic_key(mic_id: &str) -> String {
  format!("mic_not_default:{mic_id}")
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Every variant and every branch of `short_title` and of the first fix.
  fn every_issue() -> Vec<Issue> {
    let s = String::from;
    let format = SpatialFormat { id: s("{a}"), name: s("Dolby Atmos") };
    let change = OutputChange { expected_id: s("a"), expected_name: s("A"), current_id: s("b"), current_name: s("B") };
    let mut list = vec![
      Issue::OutputChanged(change),
      Issue::SpatialOff { device_id: s("o"), device_name: s("Out"), format: None },
      Issue::SpatialChanged { device_id: s("o"), device_name: s("Out"), expected: format.clone(), current: format },
      Issue::NotDefaultMic { mic_id: s("m"), mic_name: s("Mic") },
      Issue::OutputProcessing { device_name: s("Out"), key: s("k") },
    ];
    for status in [ApoStatus::Outdated, ApoStatus::NotOnThisMic, ApoStatus::NotInstalled] {
      list.push(Issue::MicProcessing { status, key: s("k") });
    }
    for why in [Silenced::Muted, Silenced::ZeroVolume, Silenced::NoMicrophone] {
      for mic_id in [Some(s("m")), None] {
        list.push(Issue::MicSilenced { why, apps: s("Discord.exe"), mic_id });
      }
    }
    for flow in [Flow::Capture, Flow::Render] {
      list.push(Issue::NotRunning { flow, device_id: s("d"), device_name: s("Dev"), key: s("k") });
    }
    list
  }

  #[test]
  fn every_issue_has_a_short_form_whose_button_is_the_menus_first_fix() {
    for issue in every_issue() {
      let short = issue.short_title();
      assert!(!short.is_empty() && short.len() <= 40, "{issue:?}: {short}");
      let (button, label, fix) = issue.first_fix().unwrap();
      assert!(!button.is_empty() && button.len() <= 14, "{issue:?}: {button}");
      assert_eq!(issue.fixes().into_iter().next(), Some((label, fix)), "{issue:?}");
    }
    // The outdated effect is one DLL for mic, output and stream EQ: no "microphone" in its words.
    let outdated = Issue::MicProcessing { status: ApoStatus::Outdated, key: String::new() };
    assert_eq!((outdated.short_title(), outdated.first_fix().unwrap().0), ("Sound effects need an update", "Update"));
    assert!(!outdated.title().to_lowercase().contains("mic"));
    // Its first fix starts the update itself (menu and flyout alike); the label says it asks for admin.
    let (label, fix) = outdated.fixes().into_iter().next().unwrap();
    assert_eq!(fix, Fix::UpdateEffects);
    assert!(label.contains("administrator") && label.contains("restarts Windows audio"), "{label}");
    let other = Issue::MicProcessing { status: ApoStatus::NotOnThisMic, key: String::new() };
    assert_eq!(other.fixes().first().map(|f| f.1.clone()), Some(Fix::OpenWindow));
  }
}
