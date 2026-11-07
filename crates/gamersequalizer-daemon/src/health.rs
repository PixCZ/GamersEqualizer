//! Setup problems the tray icon warns about (badge in `app_icon`, list + fixes in its menu):
//! - the Windows default output moved away from the one the user had (driver or Windows
//!   update, an HDMI monitor) while that one is still connected,
//! - spatial sound (Dolby Atmos, ...) switched itself off on an output where it was on, or
//!   another format (Windows Sonic instead of Dolby Atmos) took over,
//! - microphone processing (APO) missing / outdated / not on the microphone in use,
//! - the selected microphone is not the Windows default,
//! - the output EQ is on other outputs but not on the one in use (a new sound card / headset:
//!   our effect is installed per device),
//! - an app records while the microphone is muted, at 0 % or turned off (checked every second).
//!
//! Each new problem also shows a Windows notification (`notify`), at most once per
//! `NOTIFY_COOLDOWN` per problem, and not while the app's window is in front.
//!
//! "The one the user had" is learned: `HealthMemory` (`health.json` in the profiles dir) keeps
//! the last accepted default output, the outputs spatial sound was seen on and the format seen
//! first on each. "Keep" / "Ignore" in the menu and picking an output in the window update it.
//!
//! Known limit: Windows does not say who changed the default output or the spatial format, so a
//! deliberate switch in Windows itself is flagged once too; "Keep" accepts it. The window's own banner
//! (`ui/apo_banner.rs`) still checks the mic problems on its own.

use crate::i18n::{tr, trf};
use crate::mic_usage::{self, Silenced};
use crate::output_eq::settings::write_atomic;
use crate::win_audio::apo_install::{self, ApoStatus, Flow};
use crate::win_audio::dolby_atmos::{active_format, spatial_status, SpatialAudioStatus, SpatialFormat};
use crate::win_audio::shell;
use crate::win_audio::{AudioEndpointInfo, WinAudioSystem};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const FILE: &str = "health.json";
/// Device enumeration + one spatial-audio activation + a few registry reads.
const CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// A problem must show on this many checks in a row before the badge appears, so a device
/// switch in progress (Windows hops through endpoints) does not flash it.
const CONFIRM_CHECKS: u32 = 2;
/// "Recording into a muted mic" must be noticed quickly; a few registry reads + one volume read.
const MIC_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// The same problem notifies again only after this long (a mic muted on purpose mid-call).
const NOTIFY_COOLDOWN: Duration = Duration::from_secs(5 * 60);
/// Recording apps named in a title; more become "+N".
const MAX_APPS_IN_TITLE: usize = 2;

/// What the checks learned; survives restarts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HealthMemory {
  /// Default output the user had (full endpoint id).
  pub expected_output: Option<String>,
  /// Outputs on which spatial sound was seen on.
  pub spatial_on: BTreeSet<String>,
  /// Spatial format the user has on each output (first seen, or accepted with "Keep").
  pub spatial_formats: BTreeMap<String, SpatialFormat>,
  /// Problems the user chose to ignore (`Issue::ignore_key`).
  pub ignored: BTreeSet<String>,
}

/// One look at the system, input of `evaluate`.
#[derive(Debug, Clone)]
pub struct Snapshot {
  /// Active outputs.
  pub outputs: Vec<AudioEndpointInfo>,
  pub default_output: Option<String>,
  /// Spatial sound on `default_output`.
  pub spatial: SpatialAudioStatus,
  /// Which format runs on `default_output` (None: off, or Windows did not say).
  pub spatial_format: Option<SpatialFormat>,
  /// Active microphones.
  pub mics: Vec<AudioEndpointInfo>,
  /// Microphone picked in the app (None: follows the Windows default).
  pub selected_mic: Option<String>,
  pub default_mic: Option<String>,
  /// Mic processing status and the bundled build (part of the ignore key).
  pub apo: Option<(ApoStatus, String)>,
  /// Output EQ status on the output in use, and whether it is on any output at all.
  pub out_apo: Option<(ApoStatus, bool)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Issue {
  OutputChanged { expected_id: String, expected_name: String, current_id: String, current_name: String },
  /// `format`: name of the format that was on, if known.
  SpatialOff { device_id: String, device_name: String, format: Option<String> },
  SpatialChanged { device_id: String, device_name: String, expected: SpatialFormat, current: SpatialFormat },
  MicProcessing { status: ApoStatus, key: String },
  NotDefaultMic { mic_id: String, mic_name: String },
  OutputProcessing { device_name: String, key: String },
  /// `apps` already joined for display.
  MicSilenced { why: Silenced, apps: String, mic_id: Option<String> },
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
  UnmuteMic(String),
  RaiseMicVolume(String),
  /// The window's banner explains the admin prompt before installing.
  OpenWindow,
  Ignore(String),
}

/// Counts how many checks in a row each issue was seen.
#[derive(Debug, Default)]
pub struct Confirmer {
  seen: Vec<(Issue, u32)>,
}

/// Runs the checks from the service loop and applies fixes.
pub struct HealthMonitor {
  memory: HealthMemory,
  path: PathBuf,
  /// Modification time of `path` when last read or written: the window writes it too.
  file_mtime: Option<SystemTime>,
  confirmer: Confirmer,
  shown: Vec<Issue>,
  checked_at: Option<Instant>,
  mic_confirmer: Confirmer,
  mic_shown: Vec<Issue>,
  mic_checked_at: Option<Instant>,
  /// Everything last returned by `tick` (tells new problems for the log and notifications).
  announced: Vec<Issue>,
  notified_at: HashMap<String, Instant>,
}

impl Issue {
  /// One line for the menu and tooltip.
  pub fn title(&self) -> String {
    match self {
      Issue::OutputChanged { current_name, .. } => trf("Windows default output changed to \"{}\"", &[current_name]),
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
      Issue::OutputProcessing { device_name, .. } => trf("Output EQ is not set up on \"{}\"", &[device_name]),
      Issue::MicSilenced { why: Silenced::Muted, apps, .. } => {
        trf("{} is recording, but the microphone is muted", &[apps])
      }
      Issue::MicSilenced { why: Silenced::ZeroVolume, apps, .. } => {
        trf("{} is recording, but the microphone volume is 0 %", &[apps])
      }
      Issue::MicSilenced { why: Silenced::NoMicrophone, apps, .. } => {
        trf("{} is recording, but no microphone is turned on", &[apps])
      }
    }
  }

  /// Second line of the Windows notification: what it means for the user.
  pub fn detail(&self) -> &'static str {
    match self {
      Issue::OutputChanged { .. } => tr("Sound plays on another device now. The tray menu switches it back."),
      Issue::SpatialOff { .. } => tr("Turn it back on in the sound settings; the tray menu opens them."),
      Issue::SpatialChanged { .. } => {
        tr("Switch it back in the sound settings, or keep the new one; the tray menu has both.")
      }
      Issue::MicProcessing { .. } => {
        tr("Apps get your microphone without echo cancellation, noise suppression and EQ.")
      }
      Issue::NotDefaultMic { .. } => tr("Discord, games and other apps use the default microphone, not this one."),
      Issue::OutputProcessing { .. } => tr(
        "A new sound device? Your headphone correction and EQ are not applied on it. Open GamersEqualizer to set it up.",
      ),
      Issue::MicSilenced { .. } => tr("Nobody hears you. Fix it from the GamersEqualizer icon in the notification area."),
    }
  }

  /// One notification per problem and cooldown, whatever names it carries.
  fn notify_key(&self) -> String {
    match self {
      Issue::OutputChanged { current_id, .. } => format!("output:{current_id}"),
      Issue::SpatialOff { device_id, .. } => format!("spatial:{device_id}"),
      Issue::SpatialChanged { device_id, current, .. } => format!("spatial_format:{device_id}:{}", current.id),
      Issue::MicProcessing { key, .. } | Issue::OutputProcessing { key, .. } => key.clone(),
      Issue::NotDefaultMic { mic_id, .. } => not_default_mic_key(mic_id),
      Issue::MicSilenced { .. } => "mic_silenced".to_string(),
    }
  }

  /// Menu items under the title: (label, action), the fix first.
  pub fn fixes(&self) -> Vec<(String, Fix)> {
    match self {
      Issue::OutputChanged { expected_id, expected_name, current_id, current_name } => vec![
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

impl HealthMemory {
  /// Missing or corrupt file -> empty memory (everything is learned again), logged.
  pub fn load_or_default(dir: &Path) -> Self {
    let path = dir.join(FILE);
    match std::fs::read_to_string(&path) {
      Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
        warn!("[Health] {} is corrupt ({e}), starting fresh", path.display());
        Self::default()
      }),
      Err(_) => Self::default(),
    }
  }

  pub fn save(&self, dir: &Path) {
    let path = dir.join(FILE);
    let result =
      serde_json::to_string_pretty(self).map_err(std::io::Error::other).and_then(|json| write_atomic(&path, &json));
    if let Err(e) = result {
      warn!("[Health] could not save {}: {e}", path.display());
    }
  }
}

/// The user picked `output_id` in the window: it is the expected default from now on.
pub fn remember_output(dir: &Path, output_id: &str) {
  let mut memory = HealthMemory::load_or_default(dir);
  if memory.expected_output.as_deref() != Some(output_id) {
    memory.expected_output = Some(output_id.to_string());
    memory.save(dir);
  }
}

/// Spatial format the user has on `output_id`, for the window's warning.
pub fn expected_spatial(dir: &Path, output_id: &str) -> Option<SpatialFormat> {
  HealthMemory::load_or_default(dir).spatial_formats.remove(output_id)
}

/// "Keep" in the window: `format` is the user's spatial format on `output_id` from now on.
pub fn remember_spatial(dir: &Path, output_id: &str, format: &SpatialFormat) {
  let mut memory = HealthMemory::load_or_default(dir);
  if memory.spatial_formats.get(output_id) != Some(format) {
    memory.spatial_formats.insert(output_id.to_string(), format.clone());
    memory.save(dir);
  }
}

/// Problems in `snap`. Learns into `memory`; returns true as the second value when it changed.
pub fn evaluate(memory: &mut HealthMemory, snap: &Snapshot) -> (Vec<Issue>, bool) {
  let mut issues = Vec::new();
  let mut changed = false;

  if let Some(current) = snap.default_output.as_deref() {
    match memory.expected_output.as_deref() {
      None => {
        memory.expected_output = Some(current.to_string());
        changed = true;
      }
      Some(expected) if expected != current => {
        // An unplugged device is not a lost setting; it is flagged only if it comes back and
        // Windows does not switch to it.
        if let Some(e) = snap.outputs.iter().find(|d| d.id == expected) {
          issues.push(Issue::OutputChanged {
            expected_id: e.id.clone(),
            expected_name: e.name.clone(),
            current_id: current.to_string(),
            current_name: name_of(&snap.outputs, current),
          });
        }
      }
      Some(_) => {}
    }
    match snap.spatial {
      SpatialAudioStatus::Active => {
        changed |= memory.spatial_on.insert(current.to_string());
        // No format read (Windows changed its layout): on/off is still checked.
        if let Some(now) = &snap.spatial_format {
          match memory.spatial_formats.get(current) {
            None => {
              memory.spatial_formats.insert(current.to_string(), now.clone());
              changed = true;
            }
            Some(expected) if expected.id != now.id => issues.push(Issue::SpatialChanged {
              device_id: current.to_string(),
              device_name: name_of(&snap.outputs, current),
              expected: expected.clone(),
              current: now.clone(),
            }),
            Some(_) => {}
          }
        }
      }
      SpatialAudioStatus::Disabled if memory.spatial_on.contains(current) => issues.push(Issue::SpatialOff {
        device_id: current.to_string(),
        device_name: name_of(&snap.outputs, current),
        format: memory.spatial_formats.get(current).map(|f| f.name.clone()),
      }),
      // Unsupported = could not ask (device busy, COM error): no verdict.
      _ => {}
    }
  }

  if let Some((status, bundled)) = &snap.apo {
    if matches!(status, ApoStatus::NotInstalled | ApoStatus::Outdated | ApoStatus::NotOnThisMic) {
      // Bundled build in the key: a new app version asks again.
      let mic = snap.selected_mic.as_deref().or(snap.default_mic.as_deref()).unwrap_or("");
      let key = format!("apo:{status:?}:{bundled}:{mic}");
      if !memory.ignored.contains(&key) {
        issues.push(Issue::MicProcessing { status: status.clone(), key });
      }
    }
  }

  // Only where the user evidently wants it: on some output already (a new device), not "never
  // installed" (someone using the mic chain only). Outdated is the mic issue's job (one DLL).
  if let (Some((ApoStatus::NotOnThisMic, true)), Some(output)) = (&snap.out_apo, snap.default_output.as_deref()) {
    let key = format!("out_apo:{output}");
    if !memory.ignored.contains(&key) {
      issues.push(Issue::OutputProcessing { device_name: name_of(&snap.outputs, output), key });
    }
  }

  if let (Some(sel), Some(def)) = (snap.selected_mic.as_deref(), snap.default_mic.as_deref()) {
    let connected = snap.mics.iter().find(|m| m.id == sel);
    if let Some(mic) = connected.filter(|_| sel != def && !memory.ignored.contains(&not_default_mic_key(sel))) {
      issues.push(Issue::NotDefaultMic { mic_id: mic.id.clone(), mic_name: mic.name.clone() });
    }
  }

  (issues, changed)
}

impl Confirmer {
  /// Issues seen on at least `CONFIRM_CHECKS` checks in a row. Gone issues vanish at once.
  pub fn update(&mut self, now: Vec<Issue>) -> Vec<Issue> {
    let seen: Vec<(Issue, u32)> = now
      .into_iter()
      .map(|issue| {
        let before = self.seen.iter().find(|(p, _)| *p == issue).map_or(0, |(_, n)| *n);
        (issue, before.saturating_add(1))
      })
      .collect();
    self.seen = seen;
    self.seen.iter().filter(|(_, n)| *n >= CONFIRM_CHECKS).map(|(i, _)| i.clone()).collect()
  }
}

impl HealthMonitor {
  pub fn new(dir: &Path) -> Self {
    let path = dir.join(FILE);
    Self {
      memory: HealthMemory::load_or_default(dir),
      file_mtime: mtime(&path),
      path,
      confirmer: Confirmer::default(),
      shown: Vec::new(),
      checked_at: None,
      mic_confirmer: Confirmer::default(),
      mic_shown: Vec::new(),
      mic_checked_at: None,
      announced: Vec::new(),
      notified_at: HashMap::new(),
    }
  }

  /// Mic recording check every `MIC_CHECK_INTERVAL`, the rest every `CHECK_INTERVAL`; returns
  /// all issues when the shown list changed (and notifies the new ones).
  pub fn tick(&mut self, win_audio: &WinAudioSystem) -> Option<Vec<Issue>> {
    let mut changed = false;
    if self.mic_checked_at.is_none_or(|t| t.elapsed() >= MIC_CHECK_INTERVAL) {
      self.mic_checked_at = Some(Instant::now());
      let found = mic_silenced_issue(win_audio.endpoint_lock.get_default_capture_id());
      let shown = self.mic_confirmer.update(found.into_iter().collect());
      if shown != self.mic_shown {
        self.mic_shown = shown;
        changed = true;
      }
    }
    if self.checked_at.is_none_or(|t| t.elapsed() >= CHECK_INTERVAL) {
      self.checked_at = Some(Instant::now());
      changed |= self.full_check(win_audio);
    }
    if !changed {
      return None;
    }
    let all: Vec<Issue> = self.mic_shown.iter().chain(&self.shown).cloned().collect();
    self.announce(&all);
    Some(all)
  }

  /// Logs and notifies issues that were not in the last returned list.
  fn announce(&mut self, all: &[Issue]) {
    let window_in_front = crate::tray::window_in_front();
    for issue in all.iter().filter(|i| !self.announced.contains(i)) {
      info!("[Health] problem: {issue:?}");
      let key = issue.notify_key();
      let recently = self.notified_at.get(&key).is_some_and(|t| t.elapsed() < NOTIFY_COOLDOWN);
      // In front, the window's own bar already shows it.
      if !recently && !window_in_front {
        crate::notify::show(&issue.title(), issue.detail());
        self.notified_at.insert(key, Instant::now());
      }
    }
    self.announced = all.to_vec();
  }

  /// The slower checks; true when their shown list changed.
  fn full_check(&mut self, win_audio: &WinAudioSystem) -> bool {
    // The window rewrites the file when an output is picked there.
    let on_disk = mtime(&self.path);
    if on_disk != self.file_mtime {
      self.memory = HealthMemory::load_or_default(self.dir());
      self.file_mtime = on_disk;
    }

    let (issues, changed) = evaluate(&mut self.memory, &snapshot(win_audio));
    if changed {
      self.save();
    }
    let shown = self.confirmer.update(issues);
    if shown == self.shown {
      return false;
    }
    self.shown = shown;
    true
  }

  /// Runs a menu fix, then checks again on the next tick.
  pub fn apply(&mut self, fix: &Fix, win_audio: &WinAudioSystem) {
    info!("[Health] fix chosen: {fix:?}");
    let endpoints = &win_audio.endpoint_lock;
    match fix {
      Fix::SwitchOutput(id) => {
        if let Err(e) = endpoints.set_as_default_device(id) {
          warn!("[Health] switching the default output failed: {e}");
        }
      }
      Fix::KeepOutput(id) => {
        self.memory.expected_output = Some(id.clone());
        self.save();
      }
      Fix::OpenSoundSettings => shell::open_sound_settings(),
      Fix::OpenSpatialSettings => shell::open_spatial_sound_settings(),
      Fix::IgnoreSpatial(id) => {
        self.memory.spatial_on.remove(id);
        self.memory.spatial_formats.remove(id);
        self.save();
      }
      Fix::KeepSpatial(id, format) => {
        self.memory.spatial_formats.insert(id.clone(), format.clone());
        self.save();
      }
      Fix::MakeMicDefault(id) => {
        if let Err(e) = endpoints.set_as_default_recording_device(id) {
          warn!("[Health] setting the default microphone failed: {e}");
        }
      }
      Fix::UnmuteMic(id) => crate::win_audio::chatmix::set_endpoint_mute(id, false),
      Fix::RaiseMicVolume(id) => crate::win_audio::chatmix::set_endpoint_volume(id, 1.0),
      Fix::OpenWindow => crate::tray::show_or_launch_window(),
      Fix::Ignore(key) => {
        self.memory.ignored.insert(key.clone());
        self.save();
      }
    }
    self.checked_at = None;
    self.mic_checked_at = None;
  }

  fn dir(&self) -> &Path {
    self.path.parent().unwrap_or(Path::new("."))
  }

  fn save(&mut self) {
    self.memory.save(self.dir());
    self.file_mtime = mtime(&self.path);
  }
}

fn snapshot(win_audio: &WinAudioSystem) -> Snapshot {
  let endpoints = &win_audio.endpoint_lock;
  let default_output = endpoints.get_default_output_id();
  let default_mic = endpoints.get_default_capture_id();
  let selected_mic = win_audio.chatmix.mic_device_id.clone();
  let check = apo_install::check(selected_mic.as_deref().or(default_mic.as_deref()));
  // The output EQ runs on the Master output (what reaches the speakers).
  let output = win_audio.chatmix.master_device_id.clone().or_else(|| default_output.clone());
  let out_status = apo_install::check_flow(Flow::Render, output.as_deref()).status;
  // Registry scan of all render endpoints: only when it can change the verdict.
  let out_anywhere = out_status == ApoStatus::NotOnThisMic && apo_install::installed_on_any(Flow::Render);
  Snapshot {
    outputs: endpoints.enumerate_playback_devices(),
    spatial: default_output.as_deref().map_or(SpatialAudioStatus::Unsupported, spatial_status),
    spatial_format: default_output.as_deref().and_then(active_format),
    default_output,
    mics: endpoints.enumerate_capture_devices(),
    selected_mic,
    default_mic,
    apo: Some((check.status, check.bundled_version.unwrap_or_default())),
    out_apo: Some((out_status, out_anywhere)),
  }
}

pub fn mic_silenced_issue(default_mic: Option<String>) -> Option<Issue> {
  let (apps, why) = mic_usage::check_silenced(default_mic.as_deref())?;
  Some(Issue::MicSilenced { why, apps: join_apps(&apps), mic_id: default_mic })
}

/// "Discord.exe", "Discord.exe, obs64.exe", "Discord.exe, obs64.exe +2".
pub fn join_apps(apps: &[String]) -> String {
  let shown: Vec<&str> = apps.iter().take(MAX_APPS_IN_TITLE).map(String::as_str).collect();
  let rest = apps.len().saturating_sub(MAX_APPS_IN_TITLE);
  if rest > 0 {
    format!("{} +{rest}", shown.join(", "))
  } else {
    shown.join(", ")
  }
}

fn name_of(devices: &[AudioEndpointInfo], id: &str) -> String {
  devices.iter().find(|d| d.id == id).map_or_else(|| tr("Unknown device").to_string(), |d| d.name.clone())
}

fn not_default_mic_key(mic_id: &str) -> String {
  format!("mic_not_default:{mic_id}")
}

fn mtime(path: &Path) -> Option<SystemTime> {
  std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
  use super::*;

  #[test]
  fn first_default_output_is_learned_not_flagged() {
    let mut memory = HealthMemory::default();
    let (issues, changed) = evaluate(&mut memory, &snap("hp", &["hp", "tv"]));
    assert!(issues.is_empty());
    assert!(changed);
    assert_eq!(memory.expected_output.as_deref(), Some("hp"));
  }

  #[test]
  fn output_moved_away_while_expected_is_connected_is_flagged() {
    let mut memory = HealthMemory { expected_output: Some("hp".into()), ..Default::default() };
    let (issues, _) = evaluate(&mut memory, &snap("tv", &["hp", "tv"]));
    assert_eq!(issues.len(), 1);
    assert_eq!(
      issues[0].fixes()[0].1,
      Fix::SwitchOutput("hp".into()),
      "first action switches back to the expected output"
    );
  }

  #[test]
  fn unplugged_expected_output_is_not_flagged() {
    let mut memory = HealthMemory { expected_output: Some("hp".into()), ..Default::default() };
    let (issues, _) = evaluate(&mut memory, &snap("tv", &["tv"]));
    assert!(issues.is_empty());
    assert_eq!(memory.expected_output.as_deref(), Some("hp"), "kept for when it comes back");
  }

  #[test]
  fn spatial_off_is_flagged_only_where_it_was_seen_on() {
    let mut memory = HealthMemory::default();
    let mut s = snap("hp", &["hp"]);
    s.spatial = SpatialAudioStatus::Disabled;
    assert!(evaluate(&mut memory, &s).0.is_empty(), "never seen on: user's choice");

    s.spatial = SpatialAudioStatus::Active;
    evaluate(&mut memory, &s);
    s.spatial = SpatialAudioStatus::Disabled;
    let (issues, _) = evaluate(&mut memory, &s);
    assert!(matches!(issues.as_slice(), [Issue::SpatialOff { .. }]));

    s.spatial = SpatialAudioStatus::Unsupported;
    assert!(evaluate(&mut memory, &s).0.is_empty(), "no verdict when Windows can't be asked");
  }

  #[test]
  fn spatial_format_is_learned_then_a_different_one_is_flagged_until_kept() {
    let fmt = |id: &str, name: &str| SpatialFormat { id: id.into(), name: name.into() };
    let atmos = fmt("1459ac38-3875-49bf-bb59-0fe80f4d395d", "Dolby Atmos for Headphones");
    let sonic = fmt("b53d940c-b846-4831-9f76-d102b9b725a0", "Windows Sonic");
    let mut memory = HealthMemory::default();
    let mut s = snap("hp", &["hp"]);
    s.spatial = SpatialAudioStatus::Active;
    s.spatial_format = Some(atmos.clone());
    let (issues, changed) = evaluate(&mut memory, &s);
    assert!(issues.is_empty() && changed, "first format seen is the user's");

    s.spatial_format = Some(sonic.clone());
    let (issues, _) = evaluate(&mut memory, &s);
    let [issue @ Issue::SpatialChanged { .. }] = issues.as_slice() else { panic!("expected a change: {issues:?}") };
    assert!(issue.title().contains("Windows Sonic") && issue.title().contains("Dolby Atmos"));
    assert_eq!(issue.fixes()[0].1, Fix::OpenSpatialSettings);
    assert_eq!(issue.fixes()[1].1, Fix::KeepSpatial("hp".into(), sonic.clone()));

    s.spatial_format = None;
    assert!(evaluate(&mut memory, &s).0.is_empty(), "format unknown: no verdict");

    s.spatial = SpatialAudioStatus::Disabled;
    let (issues, _) = evaluate(&mut memory, &s);
    assert!(issues[0].title().contains("Dolby Atmos for Headphones"), "off names the lost format");

    memory.spatial_formats.insert("hp".into(), sonic.clone());
    s.spatial = SpatialAudioStatus::Active;
    s.spatial_format = Some(sonic);
    assert!(evaluate(&mut memory, &s).0.is_empty(), "kept");
  }

  #[test]
  fn old_memory_without_formats_still_loads() {
    let memory: HealthMemory = serde_json::from_str(r#"{"spatial_on":["hp"]}"#).unwrap();
    assert!(memory.spatial_on.contains("hp") && memory.spatial_formats.is_empty());
  }

  #[test]
  fn ignored_mic_problems_stay_hidden_until_a_new_build() {
    let mut s = snap("hp", &["hp"]);
    s.apo = Some((ApoStatus::NotInstalled, "build1".into()));
    let mut memory = HealthMemory::default();
    let (issues, _) = evaluate(&mut memory, &s);
    let Some(Fix::Ignore(key)) = issues[0].fixes().pop().map(|(_, f)| f) else { panic!("no ignore action") };
    memory.ignored.insert(key);
    assert!(evaluate(&mut memory, &s).0.is_empty());

    s.apo = Some((ApoStatus::NotInstalled, "build2".into()));
    assert_eq!(evaluate(&mut memory, &s).0.len(), 1);
  }

  #[test]
  fn selected_mic_not_default_is_flagged_while_connected() {
    let mut s = snap("hp", &["hp"]);
    s.mics = vec![dev("mic1"), dev("mic2")];
    s.selected_mic = Some("mic1".into());
    s.default_mic = Some("mic2".into());
    let mut memory = HealthMemory::default();
    assert!(matches!(evaluate(&mut memory, &s).0.as_slice(), [Issue::NotDefaultMic { .. }]));

    s.mics = vec![dev("mic2")];
    assert!(evaluate(&mut memory, &s).0.is_empty(), "unplugged mic");
  }

  #[test]
  fn output_eq_missing_on_a_new_device_is_flagged_only_if_used_elsewhere() {
    let mut memory = HealthMemory::default();
    let mut s = snap("hp", &["hp"]);
    s.out_apo = Some((ApoStatus::NotOnThisMic, false));
    assert!(evaluate(&mut memory, &s).0.is_empty(), "never set up on any output: user's choice");
    s.out_apo = Some((ApoStatus::NotOnThisMic, true));
    let (issues, _) = evaluate(&mut memory, &s);
    assert!(matches!(issues.as_slice(), [Issue::OutputProcessing { .. }]));
    let Some(Fix::Ignore(key)) = issues[0].fixes().pop().map(|(_, f)| f) else { panic!("no ignore action") };
    memory.ignored.insert(key);
    assert!(evaluate(&mut memory, &s).0.is_empty());
  }

  #[test]
  fn silenced_mic_offers_the_matching_fix() {
    let issue = |why| Issue::MicSilenced { why, apps: "Discord.exe".into(), mic_id: Some("mic".into()) };
    assert_eq!(issue(Silenced::Muted).fixes()[0].1, Fix::UnmuteMic("mic".into()));
    assert_eq!(issue(Silenced::ZeroVolume).fixes()[0].1, Fix::RaiseMicVolume("mic".into()));
    assert_eq!(issue(Silenced::NoMicrophone).fixes()[0].1, Fix::OpenSoundSettings);
    assert_eq!(issue(Silenced::Muted).notify_key(), issue(Silenced::ZeroVolume).notify_key());
  }

  #[test]
  fn recording_apps_are_shortened_for_the_title() {
    let apps = |n: usize| (1..=n).map(|i| format!("app{i}.exe")).collect::<Vec<_>>();
    assert_eq!(join_apps(&apps(1)), "app1.exe");
    assert_eq!(join_apps(&apps(2)), "app1.exe, app2.exe");
    assert_eq!(join_apps(&apps(4)), "app1.exe, app2.exe +2");
  }

  #[test]
  fn confirmer_needs_consecutive_checks_and_drops_at_once() {
    let issue = Issue::SpatialOff { device_id: "hp".into(), device_name: "HP".into(), format: None };
    let mut c = Confirmer::default();
    assert!(c.update(vec![issue.clone()]).is_empty());
    assert_eq!(c.update(vec![issue.clone()]), vec![issue.clone()]);
    assert!(c.update(vec![]).is_empty());
    assert!(c.update(vec![issue]).is_empty(), "streak restarts after a gap");
  }

  #[test]
  fn memory_survives_corrupt_file_and_roundtrips() {
    let dir = std::env::temp_dir().join(format!("ge_health_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(FILE), "{ nope").unwrap();
    assert_eq!(HealthMemory::load_or_default(&dir), HealthMemory::default());

    remember_output(&dir, "hp");
    assert_eq!(HealthMemory::load_or_default(&dir).expected_output.as_deref(), Some("hp"));
    let _ = std::fs::remove_dir_all(&dir);
  }

  fn snap(default_output: &str, outputs: &[&str]) -> Snapshot {
    Snapshot {
      outputs: outputs.iter().map(|id| dev(id)).collect(),
      default_output: Some(default_output.into()),
      spatial: SpatialAudioStatus::Unsupported,
      spatial_format: None,
      mics: Vec::new(),
      selected_mic: None,
      default_mic: None,
      apo: None,
      out_apo: None,
    }
  }

  fn dev(id: &str) -> AudioEndpointInfo {
    AudioEndpointInfo { id: id.into(), name: id.to_uppercase(), is_default: false }
  }
}
