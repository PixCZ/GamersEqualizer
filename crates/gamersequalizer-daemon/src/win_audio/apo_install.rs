//! Is an APO installed, current, and active on the endpoint in use (mic chain on the
//! microphone, output EQ on the output)? And a one-click (UAC-elevated) run of
//! `scripts/apo-install.ps1` to fix it. Both APOs are classes of the same DLL.
//!
//! "Version" is the DLL's build time: the install script copies the file with its timestamp,
//! so a bundled DLL newer than the installed one means an update is available.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use windows::core::{w, PCWSTR};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Must match CLSID_GAIN_APO / CLSID_OUTPUT_EQ_APO / CLSID_STREAM_APO in
/// crates/gamersequalizer-apo/src/lib.rs.
const MIC_APO_CLSID: &str = "{17e8d42e-dc01-4bd0-90ff-ad7c83a5ddea}";
const OUTPUT_APO_CLSID: &str = "{6b0f3c1e-8f2a-4d7b-9a51-3e2c7d4b9f10}";
const STREAM_APO_CLSID: &str = "{3c9a5e21-7d4f-4b8e-a1c6-5f2e8d9b0a47}";
const DLL_NAME: &str = "gamersequalizer_apo.dll";
const SCRIPT_NAME: &str = "apo-install.ps1";
/// Composite SFX/MFX/EFX effect lists in an endpoint's FxProperties.
const FX_LIST_VALUES: [&str; 3] = [
  "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},13",
  "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},14",
  "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},15",
];
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
  /// Bundled DLL + install script found: the fix button can work.
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

  let status = match (&endpoint_guid, installed_stamp) {
    (None, _) => ApoStatus::NoMicrophone,
    (_, None) => ApoStatus::NotInstalled,
    // Only offer an update to a newer build, never a downgrade.
    (Some(_), Some((_, installed_at))) if bundled_stamp.is_some_and(|(_, b)| b > installed_at) => ApoStatus::Outdated,
    (Some(guid), Some(_)) if !endpoint_has_apo(flow, guid) => ApoStatus::NotOnThisMic,
    _ => ApoStatus::Active,
  };

  ApoCheck {
    status,
    installed_version: installed_stamp.map(|(_, t)| format_time(t)),
    bundled_version: bundled_stamp.map(|(_, t)| format_time(t)),
    endpoint_guid,
    can_install: bundled.is_some() && install_script().is_some(),
  }
}

/// Starts the install script elevated (UAC prompt) for one microphone. Returns once the
/// window is launched; the caller re-checks afterwards.
pub fn launch_install(endpoint_guid: &str) -> Result<(), String> {
  launch_install_flow(Flow::Capture, endpoint_guid)
}

pub fn launch_install_flow(flow: Flow, endpoint_guid: &str) -> Result<(), String> {
  // The GUID came from the device id; reject anything that could break the command line.
  if !endpoint_guid.chars().all(|c| c.is_ascii_hexdigit() || "{}-".contains(c)) {
    return Err(format!("unexpected endpoint id {endpoint_guid}"));
  }
  let script = install_script().ok_or("install script not found next to the app")?;
  let dll = bundled_dll().ok_or("APO DLL not found next to the app")?;
  let params = format!(
    "-NoProfile -ExecutionPolicy Bypass -File \"{}\" -Flow {} -EndpointId \"{}\" -SourceDll \"{}\" -FromApp",
    script.display(),
    flow.as_str(),
    endpoint_guid,
    dll.display()
  );
  let params_w: Vec<u16> = params.encode_utf16().chain(std::iter::once(0)).collect();
  // SAFETY: NUL-terminated strings owned by this frame.
  let result =
    unsafe { ShellExecuteW(None, w!("runas"), w!("powershell.exe"), PCWSTR(params_w.as_ptr()), None, SW_SHOWNORMAL) };
  if result.0 as isize > SHELL_EXECUTE_OK {
    Ok(())
  } else {
    Err(format!("could not start the installer (cancelled UAC prompt? code {})", result.0 as isize))
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

fn installed_dll() -> Option<PathBuf> {
  let path = PathBuf::from(std::env::var_os("ProgramFiles")?).join("GamersEqualizer").join("apo").join(DLL_NAME);
  path.is_file().then_some(path)
}

/// The dev tree's `target/apo` build first: `target/release` may hold a stale DLL from a
/// plain `--release` build, which is not the unwinding `apo` profile (that once installed the
/// old gain-only prototype). Then next to the exe (distribution).
fn bundled_dll() -> Option<PathBuf> {
  let dir = exe_dir()?;
  [dir.join("..").join("apo").join(DLL_NAME), dir.join(DLL_NAME)].into_iter().find(|p| p.is_file())
}

fn install_script() -> Option<PathBuf> {
  let dir = exe_dir()?;
  [dir.join("scripts").join(SCRIPT_NAME), dir.join("..").join("..").join("scripts").join(SCRIPT_NAME)]
    .into_iter()
    .find(|p| p.is_file())
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
fn endpoint_guid_of(capture_id: &str) -> Option<String> {
  let (_, guid) = capture_id.rsplit_once('.')?;
  (guid.starts_with('{') && guid.ends_with('}')).then(|| guid.to_string())
}

fn endpoint_has_apo(flow: Flow, guid: &str) -> bool {
  let key =
    format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\MMDevices\\Audio\\{}\\{guid}\\FxProperties", flow.as_str());
  let lists: Vec<String> =
    FX_LIST_VALUES.iter().filter_map(|value| read_string_value(&key, value)).map(|l| l.to_ascii_lowercase()).collect();
  flow.clsids().iter().all(|clsid| lists.iter().any(|list| list.contains(clsid)))
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
    let err = launch_install("{x\" -Evil \"}").unwrap_err();
    assert!(err.contains("unexpected"), "{err}");
  }
}
