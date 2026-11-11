//! Is an APO installed, current, and active on the endpoint in use (mic chain on the
//! microphone, output EQ on the output)? And a one-click (UAC-elevated) run of
//! `scripts/apo-install.ps1` to fix it, and of `scripts/apo-uninstall.ps1` to remove it from
//! the system. Both APOs are classes of the same DLL.
//!
//! "Version" is the DLL's build time: the install script copies the file with its timestamp,
//! so a bundled DLL newer than the installed one means an update is available.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use windows::core::{w, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{
  RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_DWORD,
  RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::{
  SHGetKnownFolderPath, ShellExecuteW, FOLDERID_ProgramFiles, FOLDERID_System, KF_FLAG_DEFAULT,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Must match CLSID_GAIN_APO / CLSID_OUTPUT_EQ_APO / CLSID_STREAM_APO in
/// crates/gamersequalizer-apo/src/lib.rs.
const MIC_APO_CLSID: &str = "{17e8d42e-dc01-4bd0-90ff-ad7c83a5ddea}";
const OUTPUT_APO_CLSID: &str = "{6b0f3c1e-8f2a-4d7b-9a51-3e2c7d4b9f10}";
const STREAM_APO_CLSID: &str = "{3c9a5e21-7d4f-4b8e-a1c6-5f2e8d9b0a47}";
const ALL_CLSIDS: [&str; 3] = [MIC_APO_CLSID, OUTPUT_APO_CLSID, STREAM_APO_CLSID];
const DLL_NAME: &str = "gamersequalizer_apo.dll";
/// The app itself, installed next to its effects: `%ProgramFiles%\GamersEqualizer\<this>`.
const EXE_NAME: &str = "gamersequalizer.exe";
/// The scripts are built into the exe, so a tester gets one exe + the DLL. Written out to
/// `%LOCALAPPDATA%\GamersEqualizer\scripts` right before each elevated run.
const SCRIPT_NAME: &str = "apo-install.ps1";
const SCRIPT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/apo-install.ps1"));
const UNINSTALL_SCRIPT_NAME: &str = "apo-uninstall.ps1";
const UNINSTALL_SCRIPT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/apo-uninstall.ps1"));
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
/// Endpoint name ("Microphone Array") and its device ("Realtek(R) Audio") in `Properties`.
const ENDPOINT_NAME_VALUE: &str = "{a45c254e-df1c-4efd-8020-67d146a850e0},2";
const DEVICE_NAME_VALUE: &str = "{b3f8fa53-0004-438e-9003-51a46e139bfc},6";
/// Display names of `ALL_CLSIDS`, same order.
const CLASS_NAMES: [&str; 3] = ["Mic Chain", "Output EQ", "Stream EQ"];
/// `DEVICE_STATE_*` bits of an endpoint's `DeviceState` (higher bits are other flags).
const DEVICE_STATE_MASK: u32 = 0xF;
/// `PKEY_AudioEndpoint_Disable_SysFx`: 1 = audio enhancements off for the device.
const DISABLE_SYSFX_VALUE: &str = "{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5";
const WINDOWS_VERSION_KEY: &str = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
const AUDIO_KEY: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Audio";
/// ShellExecute returns a value > 32 on success.
const SHELL_EXECUTE_OK: isize = 32;

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
  let capture_id = endpoint_id;
  let installed = installed_dll();
  let bundled = bundled_dll();
  let installed_stamp = installed.as_deref().and_then(file_stamp);
  let bundled_stamp = bundled.as_deref().and_then(file_stamp);
  let endpoint_guid = capture_id.and_then(endpoint_guid_of);

  let dll_outdated = installed_stamp.zip(bundled_stamp).is_some_and(|((_, installed_at), (_, b))| b > installed_at);
  // The app itself is installed with its effects: a copy run from elsewhere (the unzipped
  // folder) that is newer than the installed one, or the first with this feature, updates too.
  let app = app_to_install();
  let status = match (&endpoint_guid, installed_stamp) {
    (None, _) => ApoStatus::NoMicrophone,
    (_, None) => ApoStatus::NotInstalled,
    // Only offer an update to a newer build, never a downgrade.
    // The app's own update counts only where the effect is: elsewhere it would turn "not on
    // this device" into "update", which installs the effect there.
    (Some(guid), Some(_)) if dll_outdated || (app.is_some() && endpoint_has_apo(flow, guid)) => ApoStatus::Outdated,
    (Some(guid), Some(_)) if !endpoint_has_apo(flow, guid) => ApoStatus::NotOnThisMic,
    _ => ApoStatus::Active,
  };
  // What the "outdated" message shows: the DLL builds, or the app's when only that differs.
  let (installed_at, bundled_at) = match app.filter(|_| !dll_outdated) {
    Some((installed_app, this_app)) => (installed_app, Some(this_app)),
    None => (installed_stamp.map(|(_, t)| t), bundled_stamp.map(|(_, t)| t)),
  };

  ApoCheck {
    status,
    installed_version: installed_at.map(format_time),
    bundled_version: bundled_at.map(format_time),
    endpoint_guid,
    can_install: bundled.is_some(),
  }
}

/// The installed app to start instead of this process: this exe runs from elsewhere (the
/// unzipped folder, an old autostart entry) and the installed copy is the same build or
/// newer. None in the dev tree, for the installed copy itself, and while the installed DLL is
/// missing (removed by an antivirus, a half-done uninstall): the installed copy could not put
/// it back, a copy from the ZIP can.
pub fn handover_target() -> Option<PathBuf> {
  let this = std::env::current_exe().ok()?;
  let installed = installed_exe().filter(|_| installed_dll().is_some())?;
  if same_path(&this, &installed) || in_dev_tree(this.parent()?) {
    return None;
  }
  let (_, installed_at) = file_stamp(&installed)?;
  let (_, this_at) = file_stamp(&this)?;
  (installed_at >= this_at).then_some(installed)
}

/// The exe "start with Windows" should run: the installed copy once there is one.
pub fn autostart_exe() -> Option<PathBuf> {
  let this = std::env::current_exe().ok()?;
  let installed = installed_exe().filter(|_| !this.parent().is_some_and(in_dev_tree));
  Some(installed.unwrap_or(this))
}

/// This exe still has to be installed: (installed copy's build time if any, this exe's).
/// None in the dev tree, for the installed copy itself and for an older or equal build.
fn app_to_install() -> Option<(Option<SystemTime>, SystemTime)> {
  let this = std::env::current_exe().ok()?;
  let target = installed_exe_path()?;
  if same_path(&this, &target) || in_dev_tree(this.parent()?) {
    return None;
  }
  let (_, this_at) = file_stamp(&this)?;
  let installed_at = file_stamp(&target).map(|(_, t)| t);
  installed_at.is_none_or(|t| this_at > t).then_some((installed_at, this_at))
}

/// Starts the install script elevated for every (flow, endpoint GUID): one UAC prompt and one
/// Windows audio restart. Returns once the window is launched; the caller re-checks afterwards.
pub fn launch_install_many(targets: &[(Flow, &str)]) -> Result<(), String> {
  if targets.is_empty() {
    return Ok(());
  }
  launch_installer(&endpoint_args(targets)?)
}

/// Moves the mic effect on microphone `guid` to its other slot (EFX <-> MFX), elevated. Some
/// mic arrays never start an endpoint effect (EFX) while their mode effects (MFX) run, and
/// nothing in the registry tells which in advance; Equalizer APO offers the same switch.
pub fn launch_switch_mic_slot(guid: &str) -> Result<(), String> {
  let endpoint = switch_arg(guid, mic_in_mfx(guid))?;
  log::info!("[Setup] moving the mic effect: {endpoint}");
  launch_installer(&endpoint)
}

/// The mic effect sits in the mode effect list (MFX) of microphone `guid`.
pub fn mic_in_mfx(guid: &str) -> bool {
  let key = format!("{}\\{guid}\\FxProperties", endpoints_root(Flow::Capture));
  read_string_value(&key, MFX_LIST_VALUE).is_some_and(|list| list.to_ascii_lowercase().contains(MIC_APO_CLSID))
}

/// The elevated installer uses files from the app's (user-writable) folder only when its own
/// copy hashes as given here: the DLL it installs and this exe, which it runs with
/// `--probe-mic` to check that Windows starts the mic chain.
fn launch_installer(endpoints: &str) -> Result<(), String> {
  let dll = bundled_dll().ok_or("APO DLL not found next to the app")?;
  let dll_hash = file_sha256(&dll)?;
  let own_exe = std::env::current_exe().map_err(|e| e.to_string());
  // The installer copies this exe next to the effects (not a dev build: that runs in place)
  // and runs it with --probe-mic.
  let app = match own_exe.and_then(|exe| file_sha256(&exe).map(|hash| (hash, exe))) {
    Ok((hash, exe)) => {
      let in_place = if exe.parent().is_some_and(in_dev_tree) { " -NoAppInstall" } else { "" };
      format!(" -AppExe {} -AppExeSha256 {hash}{in_place}", ps_quote(&exe.to_string_lossy()))
    }
    Err(e) => {
      log::warn!("[Setup] the installer skips the app copy and the microphone check (own exe: {e})");
      String::new()
    }
  };
  let args = format!(
    "-Endpoint {} -SourceDll {} -SourceDllSha256 {dll_hash}{app} -FromApp",
    ps_quote(endpoints),
    ps_quote(&dll.to_string_lossy())
  );
  run_script_elevated(SCRIPT_NAME, SCRIPT, &args, "installer")
}

/// Hex SHA-256 of a file (the app's exe is tens of MB: read once per install).
fn file_sha256(path: &Path) -> Result<String, String> {
  let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
  sha256_hex(&data)
}

/// `Capture:{guid}:MFX`, or `:EFX` when it is in MFX already: the slot it moves to.
fn switch_arg(guid: &str, in_mfx: bool) -> Result<String, String> {
  let to = if in_mfx { "EFX" } else { "MFX" };
  Ok(format!("{}:{to}", endpoint_args(&[(Flow::Capture, guid)])?))
}

/// `Capture:{guid},Render:{guid}` (one PowerShell array argument, no spaces).
fn endpoint_args(targets: &[(Flow, &str)]) -> Result<String, String> {
  let mut parts = Vec::with_capacity(targets.len());
  for (flow, guid) in targets {
    // The GUID came from the device id; reject anything that could break the command line.
    if guid.is_empty() || !guid.chars().all(|c| c.is_ascii_hexdigit() || "{}-".contains(c)) {
      return Err(format!("unexpected endpoint id {guid}"));
    }
    parts.push(format!("{}:{guid}", flow.as_str()));
  }
  Ok(parts.join(","))
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
  let dll = installed.as_deref().and_then(file_stamp).map_or_else(|| "missing".to_string(), |(_, t)| format_time(t));
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
  hklm_subkeys(&endpoints_root(flow)).iter().any(|guid| endpoint_has_apo(flow, guid))
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

/// Starts the uninstall script elevated (UAC prompt): removes every APO class from every
/// endpoint, the COM registration, the DLL and the install backups. Returns once launched.
pub fn launch_uninstall() -> Result<(), String> {
  run_script_elevated(UNINSTALL_SCRIPT_NAME, UNINSTALL_SCRIPT, "-RemoveBackups -FromApp", "uninstaller")
}

/// `System32\WindowsPowerShell\v1.0\powershell.exe` from the known folder, never a bare
/// `powershell.exe` (searched in the current / exe folder first) or `%SystemRoot%`
/// (user-overridable).
pub(crate) fn powershell_exe() -> Result<PathBuf, String> {
  let system = known_folder(&FOLDERID_System).ok_or("System32 folder not found")?;
  Ok(system.join("WindowsPowerShell").join("v1.0").join("powershell.exe"))
}

/// Runs a built-in script elevated with `args` (PowerShell syntax, already quoted).
/// The script is too long for the command line (`-EncodedCommand` ~33k chars, limit 32k), so
/// it goes to a user-writable file; the elevated side reads it once into memory and runs that
/// text only if its SHA-256 matches the one computed from `content` here. A same-user process
/// that swaps the file in between gets a refusal, not code running as admin. The bootstrap
/// itself travels in the command line. Module autoload is pinned to the system folder (the
/// user's Documents module folder would otherwise come first and could shadow cmdlets).
fn run_script_elevated(name: &str, content: &str, args: &str, what: &str) -> Result<(), String> {
  let path = write_script(name, content)?;
  let hash = sha256_hex(content.as_bytes())?;
  let bootstrap = elevated_bootstrap(&path.to_string_lossy(), &hash, args);
  let params = format!("-NoProfile -ExecutionPolicy Bypass -EncodedCommand {}", encode_command(&bootstrap));
  let exe = powershell_exe()?;
  let exe_w: Vec<u16> = exe.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
  let params_w: Vec<u16> = params.encode_utf16().chain(std::iter::once(0)).collect();
  // SAFETY: NUL-terminated strings owned by this frame.
  let result = unsafe {
    ShellExecuteW(None, w!("runas"), PCWSTR(exe_w.as_ptr()), PCWSTR(params_w.as_ptr()), None, SW_SHOWNORMAL)
  };
  if result.0 as isize > SHELL_EXECUTE_OK {
    Ok(())
  } else {
    Err(format!("could not start the {what} (cancelled UAC prompt? code {})", result.0 as isize))
  }
}

/// PowerShell run by `-EncodedCommand`: check the script file's hash, then run its text.
fn elevated_bootstrap(script_path: &str, sha256_hex: &str, args: &str) -> String {
  let path = ps_quote(script_path);
  [
    "$env:PSModulePath = [IO.Path]::Combine([Environment]::GetFolderPath('System'), 'WindowsPowerShell\\v1.0\\Modules')"
      .to_string(),
    format!("$c = [IO.File]::ReadAllText({path})"),
    "$h = -join ([Security.Cryptography.SHA256]::Create().ComputeHash([Text.Encoding]::UTF8.GetBytes($c)) | ForEach-Object { $_.ToString('x2') })".to_string(),
    format!(
      "if ($h -ne '{sha256_hex}') {{ Write-Host 'The script was changed after the app wrote it; not running it.' -ForegroundColor Red; Read-Host 'Press Enter to close'; exit 1 }}"
    ),
    format!("& ([scriptblock]::Create($c)) {args}"),
  ]
  .join("\n")
}

/// `-EncodedCommand` takes base64 of UTF-16LE.
fn encode_command(script: &str) -> String {
  let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
  base64(&bytes)
}

fn base64(bytes: &[u8]) -> String {
  const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
  for chunk in bytes.chunks(3) {
    let at = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
    let n = (at(0) << 16) | (at(1) << 8) | at(2);
    for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
      // `n >> shift & 63` is always < 64.
      let symbol = ALPHABET.get(((n >> shift) & 63) as usize).copied().unwrap_or(b'=');
      out.push(if i <= chunk.len() { char::from(symbol) } else { '=' });
    }
  }
  out
}

fn sha256_hex(data: &[u8]) -> Result<String, String> {
  let mut digest = [0u8; 32];
  // SAFETY: the SHA-256 pseudo-handle (Windows 10+) needs no open/close; buffers are local.
  let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut digest) };
  if status.is_err() {
    return Err(format!("SHA-256 failed (0x{:08x})", status.0));
  }
  Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// PowerShell single-quoted literal: nothing inside is expanded, `'` is doubled.
pub(crate) fn ps_quote(s: &str) -> String {
  format!("'{}'", s.replace('\'', "''"))
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
  // SAFETY: on success the shell allocates the string; it is copied, then freed once.
  unsafe {
    let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
    let path = p.to_string();
    CoTaskMemFree(Some(p.0 as *const _));
    path.ok().map(PathBuf::from)
  }
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

/// `%ProgramFiles%\GamersEqualizer` by known folder, not the variable (the user can override it).
fn program_dir() -> Option<PathBuf> {
  Some(known_folder(&FOLDERID_ProgramFiles)?.join("GamersEqualizer"))
}

fn installed_dll() -> Option<PathBuf> {
  let path = program_dir()?.join("apo").join(DLL_NAME);
  path.is_file().then_some(path)
}

/// Where the installer puts the app (whether or not it is there yet).
fn installed_exe_path() -> Option<PathBuf> {
  Some(program_dir()?.join(EXE_NAME))
}

fn installed_exe() -> Option<PathBuf> {
  installed_exe_path().filter(|path| path.is_file())
}

/// `dir` is `target/<profile>/` of the source tree (`Cargo.toml` next to `target`): a
/// developer's build, which runs in place and is never installed.
fn in_dev_tree(dir: &Path) -> bool {
  dir.parent().is_some_and(|target| {
    target.file_name().is_some_and(|n| n.eq_ignore_ascii_case("target"))
      && target.parent().is_some_and(|root| root.join("Cargo.toml").is_file())
  })
}

/// Windows paths compare without case.
fn same_path(a: &Path, b: &Path) -> bool {
  a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

/// In the dev tree the `target/apo` build first: `target/release` may hold a stale DLL from a
/// plain `--release` build, which is not the unwinding `apo` profile. The installed app
/// reinstalls its installed DLL (a new device, another slot). Otherwise only next to the exe
/// (the unzipped folder): an exe in `Downloads\x\` must not pick up whatever sits in
/// `Downloads\apo\`.
fn bundled_dll() -> Option<PathBuf> {
  let dir = exe_dir()?;
  if installed_exe_path().is_some_and(|installed| installed.parent().is_some_and(|p| same_path(p, &dir))) {
    return installed_dll();
  }
  let dev = in_dev_tree(&dir).then(|| dir.join("..").join("apo").join(DLL_NAME));
  dev.into_iter().chain([dir.join(DLL_NAME)]).find(|p| p.is_file())
}

/// Writes a built-in script fresh before every run, so an old or edited copy is never used.
/// The folder is user-writable; `run_script_elevated` checks the hash before running it.
fn write_script(name: &str, content: &str) -> Result<PathBuf, String> {
  let base = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?;
  let dir = PathBuf::from(base).join("GamersEqualizer").join("scripts");
  std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
  let path = dir.join(name);
  std::fs::write(&path, content).map_err(|e| format!("could not write {}: {e}", path.display()))?;
  Ok(path)
}

fn hklm_subkeys(key: &str) -> Vec<String> {
  /// Bound for the enumeration (a PC never has this many audio endpoints).
  const MAX_SUBKEYS: u32 = 4096;
  let key_w: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
  let mut hkey = HKEY::default();
  let mut out = Vec::new();
  // SAFETY: the handle is closed below; the name buffer is a local array of the size passed.
  unsafe {
    if RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(key_w.as_ptr()), Some(0), KEY_READ, &mut hkey) != ERROR_SUCCESS {
      return out;
    }
    for index in 0..MAX_SUBKEYS {
      let mut name = [0u16; 256];
      let mut len = name.len() as u32;
      if RegEnumKeyExW(hkey, index, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None)
        != ERROR_SUCCESS
      {
        break;
      }
      out.push(String::from_utf16_lossy(name.get(..len as usize).unwrap_or_default()));
    }
    let _ = RegCloseKey(hkey);
  }
  out
}

fn hklm_key_exists(key: &str) -> bool {
  let key_w: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
  let mut hkey = HKEY::default();
  // SAFETY: NUL-terminated key name owned by this frame; the handle is closed right away.
  unsafe {
    if RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(key_w.as_ptr()), Some(0), KEY_READ, &mut hkey) != ERROR_SUCCESS {
      return false;
    }
    let _ = RegCloseKey(hkey);
  }
  true
}

fn exe_dir() -> Option<PathBuf> {
  std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

fn file_stamp(path: &Path) -> Option<(u64, SystemTime)> {
  let meta = std::fs::metadata(path).ok()?;
  Some((meta.len(), meta.modified().ok()?))
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

fn endpoints_root(flow: Flow) -> String {
  format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\MMDevices\\Audio\\{}", flow.as_str())
}

fn endpoint_has_apo(flow: Flow, guid: &str) -> bool {
  let lists = endpoint_fx_lists(flow, guid);
  flow.clsids().iter().all(|clsid| lists.iter().any(|list| list.contains(clsid)))
}

/// The endpoint's composite effect lists, lowercased. Only these: the install script writes
/// nothing else (a legacy value is copied into the composite list, never edited).
fn endpoint_fx_lists(flow: Flow, guid: &str) -> Vec<String> {
  let key = format!("{}\\{guid}\\FxProperties", endpoints_root(flow));
  FX_LIST_VALUES.iter().filter_map(|value| read_string_value(&key, value)).map(|l| l.to_ascii_lowercase()).collect()
}

/// REG_SZ or REG_MULTI_SZ as one string (NULs become '\n').
fn read_string_value(key: &str, value: &str) -> Option<String> {
  let key_w: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
  let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
  let flags = RRF_RT_REG_SZ | RRF_RT_REG_MULTI_SZ;
  let mut size = 0u32;
  // SAFETY: first call only queries the size; the second writes into a buffer of that size.
  unsafe {
    let key_p = PCWSTR(key_w.as_ptr());
    let value_p = PCWSTR(value_w.as_ptr());
    RegGetValueW(HKEY_LOCAL_MACHINE, key_p, value_p, flags, None, None, Some(&mut size)).ok().ok()?;
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    RegGetValueW(HKEY_LOCAL_MACHINE, key_p, value_p, flags, None, Some(buf.as_mut_ptr().cast()), Some(&mut size))
      .ok()
      .ok()?;
    Some(String::from_utf16_lossy(&buf).replace('\0', "\n"))
  }
}

fn read_dword_value(key: &str, value: &str) -> Option<u32> {
  let key_w: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
  let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
  let mut data = 0u32;
  let mut size = std::mem::size_of::<u32>() as u32;
  // SAFETY: the out buffer is a local u32 of the size passed.
  unsafe {
    RegGetValueW(
      HKEY_LOCAL_MACHINE,
      PCWSTR(key_w.as_ptr()),
      PCWSTR(value_w.as_ptr()),
      RRF_RT_REG_DWORD,
      None,
      Some(&mut data as *mut u32 as *mut _),
      Some(&mut size),
    )
    .ok()
    .ok()?;
  }
  Some(data)
}

#[cfg(test)]
mod tests {
  use super::*;

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
    assert!(!SCRIPT.starts_with('\u{feff}') && !UNINSTALL_SCRIPT.starts_with('\u{feff}'));
  }

  /// Runs the bootstrap (not elevated) on a tiny script: unchanged runs with the arguments,
  /// changed is refused.
  #[test]
  fn bootstrap_runs_only_an_unchanged_script() {
    let dir = std::env::temp_dir().join(format!("ge-bootstrap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.ps1");
    let content = "param([string]$Endpoint, [switch]$FromApp)\r\n\"ran $Endpoint $FromApp\"\r\n";
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
    assert!(String::from_utf8_lossy(&ok.stdout).contains("ran Capture:{a-1},Render:{b-2} True"), "{ok:?}");
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
}
