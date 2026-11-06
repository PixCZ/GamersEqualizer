//! Setup problems the tray icon warns about (badge in `app_icon`, list + fixes in its menu):
//! - the Windows default output moved away from the one the user had (driver or Windows
//!   update, an HDMI monitor) while that one is still connected,
//! - spatial sound (Dolby Atmos, ...) switched itself off on an output where it was on,
//! - microphone processing (APO) missing / outdated / not on the microphone in use,
//! - the selected microphone is not the Windows default.
//!
//! "The one the user had" is learned: `HealthMemory` (`health.json` in the profiles dir) keeps
//! the last accepted default output and the outputs spatial sound was seen on. "Keep" /
//! "Ignore" in the menu and picking an output in the window update it.
//!
//! Known limit: Windows does not say who changed the default output, so a deliberate switch in
//! Windows itself is flagged once too; "Keep" accepts it. The window's own banner
//! (`ui/apo_banner.rs`) still checks the mic problems on its own.

use crate::i18n::{tr, trf};
use crate::output_eq::settings::write_atomic;
use crate::win_audio::apo_install::{self, ApoStatus};
use crate::win_audio::dolby_atmos::{spatial_status, SpatialAudioStatus};
use crate::win_audio::shell;
use crate::win_audio::{AudioEndpointInfo, WinAudioSystem};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const FILE: &str = "health.json";
/// Device enumeration + one spatial-audio activation + a few registry reads.
const CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// A problem must show on this many checks in a row before the badge appears, so a device
/// switch in progress (Windows hops through endpoints) does not flash it.
const CONFIRM_CHECKS: u32 = 2;

/// What the checks learned; survives restarts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HealthMemory {
  /// Default output the user had (full endpoint id).
  pub expected_output: Option<String>,
  /// Outputs on which spatial sound was seen on.
  pub spatial_on: BTreeSet<String>,
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
  /// Active microphones.
  pub mics: Vec<AudioEndpointInfo>,
  /// Microphone picked in the app (None: follows the Windows default).
  pub selected_mic: Option<String>,
  pub default_mic: Option<String>,
  /// Mic processing status and the bundled build (part of the ignore key).
  pub apo: Option<(ApoStatus, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Issue {
  OutputChanged { expected_id: String, expected_name: String, current_id: String, current_name: String },
  SpatialOff { device_id: String, device_name: String },
  MicProcessing { status: ApoStatus, key: String },
  NotDefaultMic { mic_id: String, mic_name: String },
}

/// What a menu item under an issue does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fix {
  SwitchOutput(String),
  KeepOutput(String),
  OpenSoundSettings,
  IgnoreSpatial(String),
  MakeMicDefault(String),
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
}

impl Issue {
  /// One line for the menu and tooltip.
  pub fn title(&self) -> String {
    match self {
      Issue::OutputChanged { current_name, .. } => trf("Windows default output changed to \"{}\"", &[current_name]),
      Issue::SpatialOff { device_name, .. } => trf("Spatial sound (Dolby Atmos) turned off on \"{}\"", &[device_name]),
      Issue::MicProcessing { status: ApoStatus::Outdated, .. } => tr("Microphone processing is outdated").to_string(),
      Issue::MicProcessing { status: ApoStatus::NotOnThisMic, .. } => {
        tr("Microphone processing is not on the microphone in use").to_string()
      }
      Issue::MicProcessing { .. } => tr("Microphone processing is not installed").to_string(),
      Issue::NotDefaultMic { mic_name, .. } => trf("\"{}\" is not the Windows default microphone", &[mic_name]),
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
        (tr("Open sound settings...").to_string(), Fix::OpenSoundSettings),
        (tr("Ignore").to_string(), Fix::IgnoreSpatial(device_id.clone())),
      ],
      Issue::MicProcessing { key, .. } => vec![
        (tr("Fix in GamersEqualizer...").to_string(), Fix::OpenWindow),
        (tr("Ignore").to_string(), Fix::Ignore(key.clone())),
      ],
      Issue::NotDefaultMic { mic_id, .. } => vec![
        (tr("Make it the default").to_string(), Fix::MakeMicDefault(mic_id.clone())),
        (tr("Ignore").to_string(), Fix::Ignore(not_default_mic_key(mic_id))),
      ],
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
      SpatialAudioStatus::Active => changed |= memory.spatial_on.insert(current.to_string()),
      SpatialAudioStatus::Disabled if memory.spatial_on.contains(current) => {
        issues.push(Issue::SpatialOff { device_id: current.to_string(), device_name: name_of(&snap.outputs, current) })
      }
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
    }
  }

  /// Checks every `CHECK_INTERVAL`; returns the issues when the shown list changed.
  pub fn tick(&mut self, win_audio: &WinAudioSystem) -> Option<Vec<Issue>> {
    if self.checked_at.is_some_and(|t| t.elapsed() < CHECK_INTERVAL) {
      return None;
    }
    self.checked_at = Some(Instant::now());

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
      return None;
    }
    for issue in shown.iter().filter(|i| !self.shown.contains(i)) {
      info!("[Health] problem: {issue:?}");
    }
    self.shown = shown.clone();
    Some(shown)
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
      Fix::IgnoreSpatial(id) => {
        self.memory.spatial_on.remove(id);
        self.save();
      }
      Fix::MakeMicDefault(id) => {
        if let Err(e) = endpoints.set_as_default_recording_device(id) {
          warn!("[Health] setting the default microphone failed: {e}");
        }
      }
      Fix::OpenWindow => crate::tray::show_or_launch_window(),
      Fix::Ignore(key) => {
        self.memory.ignored.insert(key.clone());
        self.save();
      }
    }
    self.checked_at = None;
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
  Snapshot {
    outputs: endpoints.enumerate_playback_devices(),
    spatial: default_output.as_deref().map_or(SpatialAudioStatus::Unsupported, spatial_status),
    default_output,
    mics: endpoints.enumerate_capture_devices(),
    selected_mic,
    default_mic,
    apo: Some((check.status, check.bundled_version.unwrap_or_default())),
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
  fn confirmer_needs_consecutive_checks_and_drops_at_once() {
    let issue = Issue::SpatialOff { device_id: "hp".into(), device_name: "HP".into() };
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
      mics: Vec::new(),
      selected_mic: None,
      default_mic: None,
      apo: None,
    }
  }

  fn dev(id: &str) -> AudioEndpointInfo {
    AudioEndpointInfo { id: id.into(), name: id.to_uppercase(), is_default: false }
  }
}
