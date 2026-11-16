//! Is an APO installed, current, and active on the endpoint in use (mic chain on the
//! microphone, output EQ on the output)? And a one-click (UAC-elevated) run of
//! `scripts/apo-install.ps1` to fix it, and of `scripts/apo-uninstall.ps1` to remove it from
//! the system. Both APOs are classes of the same DLL.
//!
//! "Version" is the DLL's build time: the install script copies the file with its timestamp,
//! so a bundled DLL newer than the installed one means an update is available.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS};
use windows::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{
  HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_DWORD, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW,
  RegGetValueW, RegOpenKeyExW,
};
use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows::Win32::UI::Shell::{
  FOLDERID_ProgramFiles, FOLDERID_System, KF_FLAG_DEFAULT, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
  SHGetKnownFolderPath, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};
use windows::core::{GUID, PCWSTR, PWSTR, w};

use crate::i18n::tr;

mod elevated;
mod location;
mod registry;

pub(crate) use elevated::*;
pub use location::*;
use registry::*;

/// Must match CLSID_MIC_APO / CLSID_OUTPUT_EQ_APO / CLSID_STREAM_APO in
/// crates/gamersequalizer-apo/src/lib.rs.
const MIC_APO_CLSID: &str = "{17e8d42e-dc01-4bd0-90ff-ad7c83a5ddea}";
const OUTPUT_APO_CLSID: &str = "{6b0f3c1e-8f2a-4d7b-9a51-3e2c7d4b9f10}";
const STREAM_APO_CLSID: &str = "{3c9a5e21-7d4f-4b8e-a1c6-5f2e8d9b0a47}";
const ALL_CLSIDS: [&str; 3] = [MIC_APO_CLSID, OUTPUT_APO_CLSID, STREAM_APO_CLSID];

/// The scripts are built into the exe, so a tester gets one exe + the DLL. Written out to
/// `%LOCALAPPDATA%\GamersEqualizer\scripts` right before each elevated run.
const SCRIPT_NAME: &str = "apo-install.ps1";
const SCRIPT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/apo-install.ps1"));
const UNINSTALL_SCRIPT_NAME: &str = "apo-uninstall.ps1";
const UNINSTALL_SCRIPT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/apo-uninstall.ps1"));
/// What both scripts share. Run from disk they load it with `COMMON_LINE`; the app puts its text
/// in that line's place (`with_common`), so the elevated run is one text with one hash.
const COMMON_SCRIPT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/apo-common.ps1"));
const COMMON_LINE: &str = r#". "$PSScriptRoot\apo-common.ps1"  # @common"#;
/// Composite MFX (mode effect) list: the mic effect moves there on mics that never start EFX.
const MFX_LIST_VALUE: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},14";
/// Composite SFX/MFX/EFX effect lists in an endpoint's FxProperties.
const FX_LIST_VALUES: [&str; 3] =
  ["{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},13", MFX_LIST_VALUE, "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},15"];
/// Legacy single-CLSID REG_SZ values that can shadow composite lists on Windows 11.
const FX_LEGACY_VALUES: [(&str, &str); 4] = [
  ("LFX", "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},1"),
  ("SFX", "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},5"),
  ("MFX", "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},6"),
  ("EFX", "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},7"),
];
/// Optional endpoint-to-APO association (`PKEY_FX_Association`).
const FX_ASSOC_VALUE: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},0";
/// Processing modes each slot runs in (`PKEY_SFX/MFX/EFX_ProcessingModes_Supported_For_Streaming`).
const FX_MODE_VALUES: [(&str, &str); 3] = [
  ("SFX", "{d3993a3f-99c2-4402-b5ec-a92a0367664b},5"),
  ("MFX", "{d3993a3f-99c2-4402-b5ec-a92a0367664b},6"),
  ("EFX", "{d3993a3f-99c2-4402-b5ec-a92a0367664b},7"),
];
const DEFAULT_PROCESSING_MODE: &str = "{c18e2f7e-933d-4965-b7d1-1eef228d2af3}";
const NULL_GUID: &str = "{00000000-0000-0000-0000-000000000000}";
/// Endpoint name ("Microphone Array") and its device ("Realtek(R) Audio") in `Properties`.
const ENDPOINT_NAME_VALUE: &str = "{a45c254e-df1c-4efd-8020-67d146a850e0},2";
const DEVICE_NAME_VALUE: &str = "{b3f8fa53-0004-438e-9003-51a46e139bfc},6";
/// Display names of `ALL_CLSIDS`, same order.
const CLASS_NAMES: [&str; 3] = ["Mic Chain", "Output EQ", "Stream EQ"];
/// `DEVICE_STATE_*` bits of an endpoint's `DeviceState` (higher bits are other flags).
const DEVICE_STATE_MASK: u32 = 0xF;
/// `DEVICE_STATE_ACTIVE`: connected and enabled.
const DEVICE_STATE_ACTIVE: u32 = 1;
/// `PKEY_AudioEndpoint_Disable_SysFx`: 1 = audio enhancements off for the device.
const DISABLE_SYSFX_VALUE: &str = "{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5";
const WINDOWS_VERSION_KEY: &str = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";

const AUDIO_KEY: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Audio";
/// Physical devices never offered for our effects unless they are the ones in use: Bluetooth
/// hands-free (call-quality mic of a headset that also has a stereo output) and VR headsets.
/// Heuristic by endpoint name, like `chatmix::is_virtual_endpoint`.
const NOT_OFFERED_NAMES: [&str; 7] =
  ["hands-free", "hands free", "handsfree", "rift", "vive", "mixed reality", "valve index"];
/// Marks the installer's rollback line (`Get-RollbackLine` in apo-install.ps1).
const ROLLED_BACK_MARK: &str = "Rolled back ";

/// Which endpoint kind / APO class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
  /// Microphone, mic chain APO.
  Capture,
  /// Headphones / speakers: output EQ APO + stream EQ APO (both installed by `-Flow Render`).
  Render,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApoStatus {
  /// Up to date and in the effect chain of the microphone in use.
  Active,
  NotInstalled,
  Outdated,
  /// Installed, but not on this microphone (e.g. a headset plugged in later).
  NotOnThisMic,
  /// No microphone to check.
  NoMicrophone,
}

/// How an elevated script runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptRun {
  /// From the window: visible, keeps errors on screen, returns once started.
  FromApp,
  /// From `--install` / `--uninstall`: waits for the exit code. `silent` (winget) hides the
  /// window and never waits for a key.
  Wait { silent: bool },
}

#[derive(Debug, Clone)]
pub struct ApoCheck {
  pub status: ApoStatus,
  /// Build time of the installed / bundled DLL ("05.11.2026 10:37").
  pub installed_version: Option<String>,
  pub bundled_version: Option<String>,
  /// Endpoint GUID of the checked microphone (`{cf0a2270-...}`).
  pub endpoint_guid: Option<String>,
  /// Bundled DLL found (the install script is built in): the fix button can work.
  pub can_install: bool,
}

/// `capture_id`: full endpoint id (`{0.0.1.00000000}.{guid}`) of the microphone in use.
pub fn check(capture_id: Option<&str>) -> ApoCheck {
  check_flow(Flow::Capture, capture_id)
}

/// `endpoint_id`: full endpoint id of the microphone (Capture) or output (Render) in use.
pub fn check_flow(flow: Flow, endpoint_id: Option<&str>) -> ApoCheck {
  let installed = installed_dll();
  let bundled = bundled_dll();
  let installed_stamp = installed.as_deref().and_then(file_stamp);
  let bundled_stamp = bundled.as_deref().and_then(file_stamp);
  let endpoint_guid = endpoint_id.and_then(endpoint_guid_of);

  let dll_outdated = installed_stamp.zip(bundled_stamp).is_some_and(|(installed_at, b)| b > installed_at);
  // The app itself is installed with its effects: a copy run from elsewhere (the unzipped
  // folder) that is newer than the installed one, or the first with this feature, updates too.
  let app = app_to_install();
  let status = match (&endpoint_guid, installed_stamp) {
    (None, _) => ApoStatus::NoMicrophone,
    (_, None) => ApoStatus::NotInstalled,
    // Only offer an update to a newer build, never a downgrade, and only where one of our effects
    // already is: elsewhere "update" would install the effect on a device the user never chose.
    (Some(guid), Some(_)) if (dll_outdated || app.is_some()) && endpoint_has_any_clsid(flow, guid) => {
      ApoStatus::Outdated
    }
    (Some(guid), Some(_)) if !endpoint_has_apo(flow, guid) => ApoStatus::NotOnThisMic,
    _ => ApoStatus::Active,
  };
  // What the "outdated" message shows: the DLL builds, or the app's when only that differs.
  let (installed_at, bundled_at) = match app.filter(|_| !dll_outdated) {
    Some((installed_app, this_app)) => (installed_app, Some(this_app)),
    None => (installed_stamp, bundled_stamp),
  };

  ApoCheck {
    status,
    installed_version: installed_at.map(format_time),
    bundled_version: bundled_at.map(format_time),
    endpoint_guid,
    can_install: bundled.is_some(),
  }
}

/// Starts the install script elevated for every (flow, endpoint GUID): one UAC prompt and one
/// Windows audio restart. Returns once the window is launched; the caller re-checks afterwards.
pub fn launch_install_many(targets: &[(Flow, &str)]) -> Result<(), String> {
  if targets.is_empty() {
    return Ok(());
  }
  launch_installer(Some(&endpoint_args(targets)?), ScriptRun::FromApp).map(drop)
}

/// `--install`: installs the app and the effects on `targets` elevated and waits; no targets =
/// the app alone (`-AppOnly`, a PC without audio devices). Returns the installer's exit code.
pub fn install_and_wait(targets: &[(Flow, &str)], silent: bool) -> Result<u32, String> {
  let endpoints = if targets.is_empty() { None } else { Some(endpoint_args(targets)?) };
  launch_installer(endpoints.as_deref(), ScriptRun::Wait { silent })?
    .ok_or_else(|| "the installer gave no exit code".to_string())
}

/// Connected endpoints that carry our effects: an update installs on them again, so the effects
/// stay where the user put them.
pub fn endpoints_with_effects() -> Vec<(Flow, String)> {
  let mut out = Vec::new();
  for flow in [Flow::Capture, Flow::Render] {
    let root = endpoints_root(flow);
    for guid in hklm_subkeys(&root) {
      let active = read_dword_value(&format!("{root}\\{guid}"), "DeviceState")
        .is_some_and(|s| s & DEVICE_STATE_MASK == DEVICE_STATE_ACTIVE);
      if active && endpoint_has_any_clsid(flow, &guid) {
        out.push((flow, guid));
      }
    }
  }
  out
}

/// Moves the mic effect on microphone `guid` to its other slot (EFX <-> MFX), elevated. Some
/// mic arrays never start an endpoint effect (EFX) while their mode effects (MFX) run, and
/// nothing in the registry tells which in advance; Equalizer APO offers the same switch.
pub fn launch_switch_mic_slot(guid: &str) -> Result<(), String> {
  let endpoint = switch_arg(guid, mic_in_mfx(guid))?;
  log::info!("[Setup] moving the mic effect: {endpoint}");
  launch_installer(Some(&endpoint), ScriptRun::FromApp).map(drop)
}

/// The mic effect sits in the mode effect list (MFX) of microphone `guid`.
pub fn mic_in_mfx(guid: &str) -> bool {
  let key = format!("{}\\{guid}\\FxProperties", endpoints_root(Flow::Capture));
  read_string_value(&key, MFX_LIST_VALUE).is_some_and(|list| list.to_ascii_lowercase().contains(MIC_APO_CLSID))
}

fn windows_build() -> Option<u32> {
  read_string_value(WINDOWS_VERSION_KEY, "CurrentBuild")?.trim().parse().ok()
}

/// One log line with what decides whether Windows runs effects on this endpoint: its effect
/// lists, the processing modes of each slot and the audio enhancements switch.
pub fn describe_endpoint(flow: Flow, guid: &str) -> String {
  let root = format!("{}\\{guid}", endpoints_root(flow));
  let fx_key = format!("{root}\\FxProperties");
  let list = |value: &str| read_string_value(&fx_key, value).map(|l| one_line(&l)).unwrap_or_default();
  let slots: Vec<String> =
    ["SFX", "MFX", "EFX"].iter().zip(FX_LIST_VALUES).map(|(slot, value)| format!("{slot} [{}]", list(value))).collect();
  let modes: Vec<String> =
    FX_MODE_VALUES.iter().map(|&(slot, value)| format!("{slot} modes [{}]", list(value))).collect();
  let legacy: Vec<String> = FX_LEGACY_VALUES
    .iter()
    .filter_map(|&(slot, value)| read_string_value(&fx_key, value).map(|v| format!("{slot} [{}]", one_line(&v))))
    .collect();
  let legacy_desc = if legacy.is_empty() { "none".to_string() } else { legacy.join(", ") };
  let assoc = read_string_value(&fx_key, FX_ASSOC_VALUE).map(|v| one_line(&v)).unwrap_or_else(|| "-".to_string());
  let off = |key: &str| read_dword_value(key, DISABLE_SYSFX_VALUE).map_or_else(|| "-".to_string(), |v| v.to_string());
  let props_key = format!("{root}\\Properties");
  format!(
    "{} {guid}: {}; {}; legacy: {legacy_desc}; assoc: {assoc}; enhancements-off flag: FxProperties {}, Properties {}",
    flow.as_str(),
    slots.join(", "),
    modes.join(", "),
    off(fx_key.as_str()),
    off(props_key.as_str())
  )
}

/// One log line: Windows build, the APO signature check switch, the installed DLL's build time.
pub fn describe_system() -> String {
  let text = |value: &str| read_string_value(WINDOWS_VERSION_KEY, value).map(|v| one_line(&v)).unwrap_or_default();
  let ubr = read_dword_value(WINDOWS_VERSION_KEY, "UBR").map_or_else(|| "?".to_string(), |v| v.to_string());
  let protected =
    read_dword_value(AUDIO_KEY, "DisableProtectedAudioDG").map_or_else(|| "-".to_string(), |v| v.to_string());
  let installed = installed_dll();
  let dll = installed.as_deref().and_then(file_stamp).map_or_else(|| "missing".to_string(), format_time);
  // Still marked as downloaded: Smart App Control / SmartScreen may refuse it.
  let zone = installed.as_deref().map_or("-", |p| if zone_marked(p) { "yes" } else { "no" });
  format!(
    "Windows {} build {}.{ubr}; DisableProtectedAudioDG {protected}; installed APO DLL {dll}, internet zone mark {zone}; \
     COM: {}",
    text("DisplayVersion"),
    text("CurrentBuild"),
    describe_registration(installed.as_deref())
  )
}

/// Every endpoint (also unplugged) whose effect lists hold our classes, with the slot(s):
/// a second sound card or an old headset shows up here.
pub fn describe_installed() -> String {
  let mut parts = Vec::new();
  for flow in [Flow::Capture, Flow::Render] {
    for guid in hklm_subkeys(&endpoints_root(flow)) {
      let endpoint = format!("{}\\{guid}", endpoints_root(flow));
      let fx = format!("{endpoint}\\FxProperties");
      let slots: Vec<&str> = ["SFX", "MFX", "EFX"]
        .into_iter()
        .zip(FX_LIST_VALUES)
        .filter(|(_, value)| {
          read_string_value(&fx, value)
            .map(|list| list.to_ascii_lowercase())
            .is_some_and(|list| flow.clsids().iter().any(|clsid| list.contains(clsid)))
        })
        .map(|(slot, _)| slot)
        .collect();
      if slots.is_empty() {
        continue;
      }
      let state = match read_dword_value(&endpoint, "DeviceState").map(|v| v & DEVICE_STATE_MASK) {
        Some(1) => "active".to_string(),
        Some(2) => "disabled".to_string(),
        Some(4) => "not present".to_string(),
        Some(8) => "unplugged".to_string(),
        Some(v) => format!("state {v}"),
        None => "state ?".to_string(),
      };
      let props = format!("{endpoint}\\Properties");
      let name = |value| read_string_value(&props, value).map(|n| one_line(&n)).unwrap_or_default();
      let (endpoint_name, device) = (name(ENDPOINT_NAME_VALUE), name(DEVICE_NAME_VALUE));
      parts.push(format!("{} {guid} \"{endpoint_name} ({device})\" {state}: {}", flow.as_str(), slots.join("+")));
    }
  }
  if parts.is_empty() {
    return "our effects are on no device".to_string();
  }
  format!("our effects are on: {}", parts.join("; "))
}

/// "Mic Chain ok, Output EQ ok, Stream EQ points to C:\old\x.dll": audiodg creates a class only
/// when it points at the installed DLL and is registered as an APO.
fn describe_registration(dll: Option<&Path>) -> String {
  let dll = dll.map(|d| d.to_string_lossy().to_ascii_lowercase());
  let states = ALL_CLSIDS.iter().zip(CLASS_NAMES).map(|(clsid, name)| {
    let server = read_string_value(&format!("SOFTWARE\\Classes\\CLSID\\{clsid}\\InprocServer32"), "");
    let apo = hklm_key_exists(&format!("SOFTWARE\\Classes\\AudioEngine\\AudioProcessingObjects\\{clsid}"));
    let state = match server.map(|s| s.trim().to_string()) {
      None => "not registered".to_string(),
      Some(path) if dll.as_deref() != Some(path.to_ascii_lowercase().as_str()) => format!("points to {path}"),
      Some(_) if !apo => "no APO entry".to_string(),
      Some(_) => "ok".to_string(),
    };
    format!("{name} {state}")
  });
  states.collect::<Vec<_>>().join(", ")
}

/// The `Zone.Identifier` stream Windows adds to downloaded files.
fn zone_marked(path: &Path) -> bool {
  std::fs::metadata(format!("{}:Zone.Identifier", path.display())).is_ok()
}

/// A REG_MULTI_SZ read by `read_string_value` on one line: "a | b".
fn one_line(value: &str) -> String {
  value.split('\n').filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" | ")
}

/// Is this flow's APO in the effect lists of any endpoint (also unplugged ones)? Tells a new
/// sound card the user's effect is missing on apart from "never wanted it there".
pub fn installed_on_any(flow: Flow) -> bool {
  hklm_subkeys(&endpoints_root(flow)).iter().any(|guid| endpoint_has_any_clsid(flow, guid))
}

/// Active physical endpoints in `endpoints` that lack our effect (not installed, or shadowed by
/// legacy values: `!endpoint_has_apo`). One without `FxProperties` gets the key from the
/// installer. Virtual devices, Bluetooth hands-free and VR headsets are left out. Returns
/// `(guid, name)` pairs; the setup card offers them unticked next to the devices in use.
pub fn targets_to_install(flow: Flow, endpoints: &[super::AudioEndpointInfo]) -> Vec<(String, String)> {
  let eligible: Vec<(&super::AudioEndpointInfo, String)> = endpoints
    .iter()
    .filter(|ep| offered_for_effects(&ep.name.to_lowercase()))
    .filter_map(|ep| Some((ep, endpoint_guid_of(&ep.id)?)))
    .filter(|(_, guid)| !endpoint_has_apo(flow, guid))
    .collect();
  dedup_target_guids(eligible.into_iter().map(|(ep, guid)| (guid, ep.name.clone())))
}

fn offered_for_effects(name_lowercase: &str) -> bool {
  !super::chatmix::is_virtual_endpoint(name_lowercase) && !NOT_OFFERED_NAMES.iter().any(|n| name_lowercase.contains(n))
}

/// `Capture:{guid}` (GUID lowercased): how the installer names an endpoint in its rollback line.
pub fn endpoint_key(flow: Flow, guid: &str) -> String {
  format!("{}:{}", flow.as_str(), guid.to_ascii_lowercase())
}

/// The installer undid its change on an endpoint (probe failed, audiodg crashed): `(note time,
/// endpoint_key)` from its rollback line in the notes ("time installer: ... Rolled back NAME
/// [Capture:{guid}] ..."), else None.
pub fn rolled_back_endpoint(note: &str) -> Option<(String, String)> {
  let (time, text) = note.split_once(' ')?;
  let rest = text.strip_prefix("installer: ")?.trim_start();
  let after = rest.strip_prefix(ROLLED_BACK_MARK)?;
  let (_, tagged) = after.rsplit_once('[')?;
  let (inner, _) = tagged.split_once(']')?;
  let (flow, guid) = inner.split_once(':')?;
  let flow = match flow {
    "Capture" => Flow::Capture,
    "Render" => Flow::Render,
    _ => return None,
  };
  let well_formed =
    guid.starts_with('{') && guid.ends_with('}') && guid.chars().all(|c| c.is_ascii_hexdigit() || "{}-".contains(c));
  // To the second: "...01.5Z" and "...01Z" must not compare as text with the fraction.
  let time = time.get(..19).unwrap_or(time);
  well_formed.then(|| (time.to_string(), endpoint_key(flow, guid)))
}

fn dedup_target_guids(items: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
  let mut seen = Vec::<String>::new();
  let mut out = Vec::new();
  for (guid, name) in items {
    let lower = guid.to_ascii_lowercase();
    if !seen.contains(&lower) {
      seen.push(lower);
      out.push((guid, name));
    }
  }
  out
}

/// Anything of ours left in the system: the installed DLL, a registered COM class, or one of
/// our CLSIDs in any endpoint's effect lists (also unplugged ones). The last one catches a
/// half-done removal (DLL and classes gone, effect lists not), which would leave endpoints
/// with an effect Windows can't load.
pub fn is_registered() -> bool {
  installed_dll().is_some()
    || installed_exe().is_some()
    || ALL_CLSIDS.iter().any(|clsid| hklm_key_exists(&format!("SOFTWARE\\Classes\\CLSID\\{clsid}")))
    || [Flow::Capture, Flow::Render].into_iter().any(|flow| {
      hklm_subkeys(&endpoints_root(flow))
        .iter()
        .any(|guid| endpoint_fx_lists(flow, guid).iter().any(|list| ALL_CLSIDS.iter().any(|c| list.contains(c))))
    })
}

/// Starts the uninstall script elevated (UAC prompt): puts back each endpoint's effect settings
/// from the installer's backups (`%ProgramFiles%\GamersEqualizer\apo-backup`), removes every APO
/// class from every endpoint (plus Default-only streaming modes on emptied slots of endpoints
/// without a backup), the COM registration, the DLL, the backups once restored, any legacy
/// backups and temporary scripts in `%LOCALAPPDATA%\GamersEqualizer\scripts`.
/// Returns once launched, or with the exit code when `run` waits.
pub fn launch_uninstall(run: ScriptRun) -> Result<Option<u32>, String> {
  let args = uninstall_args(scripts_dir().as_deref(), run);
  run_script_elevated(UNINSTALL_SCRIPT_NAME, UNINSTALL_SCRIPT, &args, "uninstaller", run)
}

impl Flow {
  fn as_str(self) -> &'static str {
    match self {
      Flow::Capture => "Capture",
      Flow::Render => "Render",
    }
  }

  /// Classes that must all be in the endpoint's effect lists.
  fn clsids(self) -> &'static [&'static str] {
    match self {
      Flow::Capture => &[MIC_APO_CLSID],
      Flow::Render => &[OUTPUT_APO_CLSID, STREAM_APO_CLSID],
    }
  }
}

fn format_time(t: SystemTime) -> String {
  jiff::Timestamp::try_from(t)
    .map(|ts| ts.to_zoned(jiff::tz::TimeZone::system()).strftime("%d.%m.%Y %H:%M").to_string())
    .unwrap_or_else(|_| "?".to_string())
}

/// `{0.0.1.00000000}.{cf0a2270-...}` -> `{cf0a2270-...}`
pub(crate) fn endpoint_guid_of(capture_id: &str) -> Option<String> {
  let (_, guid) = capture_id.rsplit_once('.')?;
  (guid.starts_with('{') && guid.ends_with('}')).then(|| guid.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The scripts spell these constants themselves: a mismatch installs where the app never looks.
  #[test]
  fn scripts_use_the_same_constants() {
    let has = |script: &str, text: &str| script.to_lowercase().contains(&text.to_lowercase());
    let fx_guid = FX_ASSOC_VALUE.split(',').next().unwrap();
    let modes_guid = FX_MODE_VALUES[0].1.split(',').next().unwrap();
    let (install, uninstall) = (with_common(SCRIPT), with_common(UNINSTALL_SCRIPT));
    for script in [&install, &uninstall] {
      assert!(has(script, &format!("$FxGuid = '{fx_guid}'")));
      assert!(has(script, &format!("$ModesGuid = '{modes_guid}'")));
      for text in ALL_CLSIDS.iter().chain(&[NULL_GUID, DEFAULT_PROCESSING_MODE]) {
        assert!(has(script, &format!("'{text}'")), "{text}");
      }
    }
    let values = FX_LIST_VALUES.iter().chain(FX_LEGACY_VALUES.iter().chain(&FX_MODE_VALUES).map(|(_, v)| v));
    for value in values {
      let text = format!("\"{}\"", value.replace(fx_guid, "$FxGuid").replace(modes_guid, "$ModesGuid"));
      assert!(has(&install, &text), "{text}");
    }
    for text in [ENDPOINT_NAME_VALUE, DEVICE_NAME_VALUE, DISABLE_SYSFX_VALUE] {
      assert!(has(&install, &format!("'{text}'")), "{text}");
    }
    assert!(has(&install, &format!("DeviceState -band {DEVICE_STATE_MASK:#X}) -eq {DEVICE_STATE_ACTIVE}")));
  }

  #[test]
  fn endpoint_guid_is_taken_from_the_device_id() {
    assert_eq!(
      endpoint_guid_of("{0.0.1.00000000}.{cf0a2270-1885-4dd6-82a5-f636e9efadd5}").as_deref(),
      Some("{cf0a2270-1885-4dd6-82a5-f636e9efadd5}")
    );
    assert_eq!(endpoint_guid_of("garbage"), None);
  }

  #[test]
  fn install_refuses_ids_that_could_inject_arguments() {
    let err = launch_install_many(&[(Flow::Capture, "{x\" -Evil \"}")]).unwrap_err();
    assert!(err.contains("unexpected"), "{err}");
    let err = launch_install_many(&[(Flow::Capture, "{a}"), (Flow::Render, "")]).unwrap_err();
    assert!(err.contains("unexpected"), "{err}");
  }

  #[test]
  fn base64_matches_known_vectors() {
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
    assert_eq!(base64(b"foobar"), "Zm9vYmFy");
  }

  #[test]
  fn sha256_matches_known_vector() {
    assert_eq!(sha256_hex(b"abc").unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
  }

  #[test]
  fn bootstrap_quotes_the_path_and_fits_the_command_line() {
    let b = elevated_bootstrap(r"C:\Users\o'neil\x.ps1", "ab", "-FromApp");
    assert!(b.contains(r"'C:\Users\o''neil\x.ps1'"), "{b}");
    assert!(encode_command(&b).len() < 4096);
  }

  #[test]
  fn scripts_have_no_bom_so_the_elevated_hash_matches() {
    // `ReadAllText` drops a BOM, `include_str!` keeps it: the hashes would never match.
    assert!([SCRIPT, UNINSTALL_SCRIPT, COMMON_SCRIPT].iter().all(|s| !s.starts_with('\u{feff}')));
  }

  #[test]
  fn both_scripts_get_the_common_part_once() {
    for script in [SCRIPT, UNINSTALL_SCRIPT] {
      assert_eq!(script.matches(COMMON_LINE).count(), 1);
      let full = with_common(script);
      assert!(!full.contains(COMMON_LINE) && full.contains(COMMON_SCRIPT));
    }
  }

  /// Runs the bootstrap (not elevated) on a tiny script: unchanged runs with the arguments,
  /// changed is refused.
  #[test]
  fn bootstrap_runs_only_an_unchanged_script() {
    let dir = std::env::temp_dir().join(format!("ge-bootstrap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.ps1");
    // `Get-Seen` reads a top-level variable the way the installer's functions do (`$script:`).
    let content = "param([string]$Endpoint, [switch]$FromApp)\r\n$Seen = @{ a = 1 }\r\nfunction Get-Seen { $script:Seen['a'] }\r\n\"ran $Endpoint $FromApp $(Get-Seen)\"\r\n";
    let run = |file_text: &str| {
      std::fs::write(&path, file_text).unwrap();
      let hash = sha256_hex(content.as_bytes()).unwrap();
      let args = format!("-Endpoint {} -FromApp", ps_quote("Capture:{a-1},Render:{b-2}"));
      let b = elevated_bootstrap(&path.to_string_lossy(), &hash, &args);
      std::process::Command::new(powershell_exe().unwrap())
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encode_command(&b)])
        .output()
        .unwrap()
    };
    let ok = run(content);
    assert!(String::from_utf8_lossy(&ok.stdout).contains("ran Capture:{a-1},Render:{b-2} True 1"), "{ok:?}");
    let swapped = run(&content.replace("ran", "evil"));
    assert!(!swapped.status.success() && !String::from_utf8_lossy(&swapped.stdout).contains("evil"), "{swapped:?}");
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn several_endpoints_become_one_script_argument() {
    let args = endpoint_args(&[(Flow::Capture, "{cf0a-1}"), (Flow::Render, "{b45c-2}")]).unwrap();
    assert_eq!(args, "Capture:{cf0a-1},Render:{b45c-2}");
  }

  #[test]
  fn switching_the_mic_slot_names_the_other_one() {
    assert_eq!(switch_arg("{cf0a-1}", false).unwrap(), "Capture:{cf0a-1}:MFX");
    assert_eq!(switch_arg("{cf0a-1}", true).unwrap(), "Capture:{cf0a-1}:EFX");
    assert!(switch_arg("{x' -Evil '}", false).is_err());
  }

  #[test]
  fn endpoint_config_requires_default_mode_and_no_shadowing_legacy() {
    let mode = Some("{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}".to_string());
    // EPOS Capture before fix: ,15 has Mic Chain, modes,7 is missing -> incomplete.
    assert!(!endpoint_fx_configured(
      Flow::Capture,
      &[None, None, Some(MIC_APO_CLSID.into())],
      &[None, None, None],
      &[None, None, None, None],
    ));
    // With modes,7 present -> ready (and MFX with modes,6 -> ready).
    assert!(endpoint_fx_configured(
      Flow::Capture,
      &[None, None, Some(MIC_APO_CLSID.into())],
      &[None, None, mode.clone()],
      &[None, None, None, Some(NULL_GUID.into())],
    ));
    assert!(endpoint_fx_configured(
      Flow::Capture,
      &[None, Some(MIC_APO_CLSID.into()), None],
      &[None, mode.clone(), None],
      &[None, None, None, None],
    ));
    // Render with legacy ,5/,6 shadowing ,13/,14 -> incomplete even with modes!
    assert!(!endpoint_fx_configured(
      Flow::Render,
      &[Some(STREAM_APO_CLSID.into()), None, Some(OUTPUT_APO_CLSID.into())],
      &[mode.clone(), None, mode.clone()],
      &[None, Some("{C9453E73-8C5C-4463-9984-AF8BAB2F5447}".into()), None, None],
    ));
    // Render cleanly migrated -> ready.
    assert!(endpoint_fx_configured(
      Flow::Render,
      &[Some(STREAM_APO_CLSID.into()), None, Some(OUTPUT_APO_CLSID.into())],
      &[mode.clone(), None, mode.clone()],
      &[None, None, None, None],
    ));
    // A legacy value shadows only its own slot (measured): MFX ,6 leaves SFX ,13 and EFX ,15
    // running, but not MFX ,14; the old pre-mix value LFX ,1 counts as SFX.
    let mfx_legacy = Some("{13AB3EBD-137E-4903-9D89-60BE8277FD17}".to_string());
    assert!(endpoint_fx_configured(
      Flow::Render,
      &[Some(STREAM_APO_CLSID.into()), None, Some(OUTPUT_APO_CLSID.into())],
      &[mode.clone(), None, mode.clone()],
      &[None, None, mfx_legacy.clone(), None],
    ));
    assert!(!endpoint_fx_configured(
      Flow::Render,
      &[Some(STREAM_APO_CLSID.into()), Some(OUTPUT_APO_CLSID.into()), None],
      &[mode.clone(), mode.clone(), None],
      &[None, None, mfx_legacy.clone(), None],
    ));
    assert!(!endpoint_fx_configured(
      Flow::Render,
      &[Some(STREAM_APO_CLSID.into()), None, Some(OUTPUT_APO_CLSID.into())],
      &[mode.clone(), None, mode.clone()],
      &[mfx_legacy, None, None, None],
    ));
    // Capture in EFX next to the driver's legacy MFX: ready.
    assert!(endpoint_fx_configured(
      Flow::Capture,
      &[None, None, Some(MIC_APO_CLSID.into())],
      &[None, None, mode],
      &[None, None, Some("{13AB3EBD-137E-4903-9D89-60BE8277FD17}".into()), None],
    ));
  }

  #[test]
  fn rollback_lines_of_the_installer_name_the_endpoint() {
    let note = "2026-11-12T10:00:01.5Z installer: Rolled back Headset (USB Audio) [Capture:{CF0A2270-1885-4dd6-82a5-f636e9efadd5}] to its pre-install effect settings.";
    assert_eq!(
      rolled_back_endpoint(note),
      Some(("2026-11-12T10:00:01".to_string(), "Capture:{cf0a2270-1885-4dd6-82a5-f636e9efadd5}".to_string()))
    );
    // Brackets in the device name: the last pair is the endpoint.
    let note = "t installer:   Rolled back Mic [Front] (X) [Render:{b45cef56-a56b-49b0-88c0-4af057026718}] to its x.";
    let key = rolled_back_endpoint(note).map(|(_, k)| k);
    assert_eq!(key.as_deref(), Some("Render:{b45cef56-a56b-49b0-88c0-4af057026718}"));
    for other in [
      "t installer: Target: Mic {cf0a2270-1885-4dd6-82a5-f636e9efadd5}: GamersEqualizer Mic Chain (EFX)",
      "t audiodg.exe pid 1: Rolled back Mic [Capture:{cf0a2270-1885-4dd6-82a5-f636e9efadd5}]",
      "t installer: Rolled back Mic [Speaker:{cf0a2270-1885-4dd6-82a5-f636e9efadd5}]",
      "t installer: Rolled back Mic [Capture:{cf0a2270'; evil}]",
      "",
    ] {
      assert_eq!(rolled_back_endpoint(other), None, "{other}");
    }
  }

  #[test]
  fn hands_free_vr_and_virtual_devices_are_not_offered() {
    for name in ["headset (wh-1000xm4 hands-free ag audio)", "rift s", "nvidia broadcast", "voicemeeter input"] {
      assert!(!offered_for_effects(name), "{name}");
    }
    assert!(offered_for_effects("headphones (jabra evolve2 65)"));
  }

  #[test]
  fn dedup_target_guids_drops_case_duplicates_keeping_first() {
    let out = dedup_target_guids([
      ("{AAAA-1}".into(), "One".into()),
      ("{aaaa-1}".into(), "One Dup".into()),
      ("{BBBB-2}".into(), "Two".into()),
    ]);
    assert_eq!(out, vec![("{AAAA-1}".into(), "One".into()), ("{BBBB-2}".into(), "Two".into())]);
  }

  #[test]
  fn uninstall_args_quotes_script_dir_and_fits_command_line() {
    let args =
      uninstall_args(Some(Path::new(r"C:\Users\O'Neil\AppData\Local\GamersEqualizer\scripts")), ScriptRun::FromApp);
    assert_eq!(args, r"-RemoveBackups -FromApp -ScriptDir 'C:\Users\O''Neil\AppData\Local\GamersEqualizer\scripts'");
    assert_eq!(uninstall_args(None, ScriptRun::Wait { silent: true }), "-RemoveBackups");
    assert_eq!(uninstall_args(None, ScriptRun::Wait { silent: false }), "-RemoveBackups -FromApp");
    let b = elevated_bootstrap(r"C:\Users\O'Neil\AppData\Local\GamersEqualizer\scripts\apo-uninstall.ps1", "ab", &args);
    assert!(encode_command(&b).len() < 4096);
  }

  #[test]
  fn cleanup_staged_scripts_removes_scripts_and_preserves_siblings() {
    let root = std::env::temp_dir().join(format!("ge-script-clean-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let app = root.join("GamersEqualizer");
    let scripts = app.join("scripts");
    let logs = app.join("usb-apo-logs");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(scripts.join(SCRIPT_NAME), "a").unwrap();
    std::fs::write(scripts.join(UNINSTALL_SCRIPT_NAME), "b").unwrap();

    cleanup_staged_scripts_in(&scripts);
    assert!(!scripts.exists(), "empty scripts dir should be removed");
    assert!(logs.exists(), "sibling dirs must stay untouched");

    std::fs::remove_dir(&logs).unwrap();
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::write(scripts.join(UNINSTALL_SCRIPT_NAME), "c").unwrap();
    cleanup_staged_scripts_in(&scripts);
    assert!(!app.exists(), "empty parent GamersEqualizer dir should be removed");
    let _ = std::fs::remove_dir_all(&root);
  }
}
