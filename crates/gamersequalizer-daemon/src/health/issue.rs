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
  Ignore(String),
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
      Issue::MicProcessing { status: ApoStatus::Outdated, .. } => tr("Microphone processing is outdated").to_string(),
      Issue::MicProcessing { status: ApoStatus::NotOnThisMic, .. } => {
        tr("Microphone processing is not on the microphone in use").to_string()
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
      Issue::OutputChanged(_) => tr("Sound plays on another device now. The tray menu switches it back."),
      Issue::SpatialOff { .. } => tr("Turn it back on in the sound settings; the tray menu opens them."),
      Issue::SpatialChanged { .. } => {
        tr("Switch it back in the sound settings, or keep the new one; the tray menu has both.")
      }
      Issue::MicProcessing { status: ApoStatus::Outdated, .. } => tr("Newer features do not work until you update it."),
      Issue::MicProcessing { .. } => {
        tr("Apps get your microphone without echo cancellation, noise suppression and EQ.")
      }
      Issue::NotDefaultMic { .. } => tr("Discord, games and other apps use the default microphone, not this one."),
      Issue::OutputProcessing { .. } => tr(
        "A new sound device? Open GamersEqualizer to set it up: headphone correction, EQ and echo cancellation need it.",
      ),
      Issue::MicSilenced { .. } => {
        tr("Nobody hears you. Fix it from the GamersEqualizer icon in the notification area.")
      }
      // Only what was seen: it processed nothing. Why is in the log ([ApoNote], [ApoLink]).
      Issue::NotRunning { flow: Flow::Capture, .. } => tr(
        "It is installed, but processed nothing while apps recorded: they get the raw microphone. The reason is in the app's log.",
      ),
      Issue::NotRunning { flow: Flow::Render, .. } => tr(
        "Our effect is installed, but processed nothing while sound played: no EQ, no echo cancellation. The reason \
           is in the app's log.",
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
