//! "Is any app recording from a microphone right now?", read the way the Windows privacy
//! indicator does it: every app that opened a mic has a key under ConsentStore\microphone, and
//! `LastUsedTimeStop == 0` (with a start time) means its capture stream is still open, even
//! while the mic is silent. Covers every microphone and works without our APO.
//!
//! Also: "an app records, but the microphone is muted / at 0 % / gone" (`silenced`), the
//! classic "I talked for a minute and nobody heard me". The tray and the window warn about it.

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
  RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_QWORD,
};

const MIC_STORE: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";
/// Desktop (non-Store) apps live one level deeper, one key per exe path with `\` written as `#`.
const NON_PACKAGED: &str = "NonPackaged";

/// Windows volume at or below this is "off" (the slider at 0 or 1 %).
const SILENT_LEVEL: f32 = 0.015;

/// Why apps that record get no voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Silenced {
  Muted,
  ZeroVolume,
  /// No active microphone at all (disabled in Windows or unplugged).
  NoMicrophone,
}

/// Apps recording from the mic in use (`mic_id`: Windows default capture endpoint) while it
/// is muted, at 0 % or missing. None while nobody records or the mic is fine.
pub fn check_silenced(mic_id: Option<&str>) -> Option<(Vec<String>, Silenced)> {
  let apps = recording_apps();
  if apps.is_empty() {
    return None;
  }
  let level = mic_id.and_then(crate::win_audio::chatmix::get_endpoint_volume);
  silenced(mic_id.is_some(), level).map(|why| (apps, why))
}

/// Verdict for a recording in progress. `level`: (volume 0..1, muted); None = could not read
/// it (device busy, COM error): no verdict rather than a false alarm.
pub fn silenced(mic_present: bool, level: Option<(f32, bool)>) -> Option<Silenced> {
  if !mic_present {
    return Some(Silenced::NoMicrophone);
  }
  match level? {
    (_, true) => Some(Silenced::Muted),
    (volume, false) if volume <= SILENT_LEVEL => Some(Silenced::ZeroVolume),
    _ => None,
  }
}

/// True when some app other than this one keeps a microphone open.
pub fn microphone_in_use() -> bool {
  !recording_apps().is_empty()
}

/// A scan this young is reused: the tray and the health check both ask twice a second, and the
/// scan walks every key under ConsentStore (which only grows, one per old Discord version).
const SCAN_MAX_AGE: std::time::Duration = std::time::Duration::from_millis(400);

/// Apps other than this one that keep a microphone open right now, as short display names
/// (`Discord.exe`, `WindowsSoundRecorder`), sorted, without duplicates.
pub fn recording_apps() -> Vec<String> {
  let mut apps: Vec<String> = recording().into_iter().map(|(_, name)| name).collect();
  apps.sort_by_key(|a| a.to_lowercase());
  apps.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
  apps
}

/// The app that has kept a microphone open longest. With several mic chains running (apps
/// recording in different processing modes) the first chain's values are the ones shown, and
/// it belongs to this app.
/// shortcut: any microphone counts, and a third app sharing the first chain's mode keeps it
/// alive after its first app stopped; exact only with one mic and two apps. Upgrade: the APO
/// reports its stream's session, the service matches it to an app.
pub fn longest_recording_app() -> Option<String> {
  recording().into_iter().min_by_key(|(since, _)| *since).map(|(_, name)| name)
}

/// (recording since, display name) of each app that keeps a microphone open; see `SCAN_MAX_AGE`.
fn recording() -> Vec<(u64, String)> {
  use std::sync::Mutex;
  use std::time::Instant;
  type Scan = (Instant, Vec<(u64, String)>);
  static LAST: Mutex<Option<Scan>> = Mutex::new(None);
  let Ok(mut last) = LAST.lock() else { return scan_recording_apps() };
  if let Some((at, apps)) = last.as_ref() {
    if at.elapsed() < SCAN_MAX_AGE {
      return apps.clone();
    }
  }
  let apps = scan_recording_apps();
  *last = Some((Instant::now(), apps.clone()));
  apps
}

fn scan_recording_apps() -> Vec<(u64, String)> {
  // Our own mic probe (`--probe-mic`) opens the mic too; it is not "another app recording".
  let own = std::env::current_exe().ok().map(|p| p.to_string_lossy().replace('\\', "#").to_lowercase());
  let is_own = |name: &str| own.as_deref() == Some(name.to_lowercase().as_str());

  let store = HSTRING::from(MIC_STORE);
  let Some(root) = open_key(HKEY_CURRENT_USER, &store) else { return Vec::new() };
  let mut apps = Vec::new();
  for name in subkey_names(root) {
    if name == NON_PACKAGED {
      if let Some(np) = open_key(root, &HSTRING::from(NON_PACKAGED)) {
        for app in subkey_names(np) {
          if let Some(since) = in_use_since(np, &app).filter(|_| !is_own(&app)) {
            apps.push((since, display_name(&app, true)));
          }
        }
        close_key(np);
      }
    } else if let Some(since) = in_use_since(root, &name) {
      apps.push((since, display_name(&name, false)));
    }
  }
  close_key(root);
  apps
}

/// `C:#Apps#Discord.exe` -> `Discord.exe`; Store package `Microsoft.WindowsSoundRecorder_8wekyb3d8bbwe`
/// -> `WindowsSoundRecorder` (publisher prefix and hash dropped).
fn display_name(key: &str, non_packaged: bool) -> String {
  if non_packaged {
    return key.rsplit('#').next().unwrap_or(key).to_string();
  }
  let family = key.split('_').next().unwrap_or(key);
  family.rsplit('.').next().filter(|s| !s.is_empty()).unwrap_or(family).to_string()
}

/// When the app opened the microphone it still holds (FILETIME), None when it holds none.
fn in_use_since(parent: HKEY, app: &str) -> Option<u64> {
  let key = HSTRING::from(app);
  let start = read_qword(parent, &key, "LastUsedTimeStart")?;
  let stop = read_qword(parent, &key, "LastUsedTimeStop")?;
  (start != 0 && stop == 0).then_some(start)
}

fn read_qword(parent: HKEY, subkey: &HSTRING, value: &str) -> Option<u64> {
  let mut data = 0u64;
  let mut size = std::mem::size_of::<u64>() as u32;
  let res = unsafe {
    RegGetValueW(
      parent,
      subkey,
      &HSTRING::from(value),
      RRF_RT_REG_QWORD,
      None,
      Some(&mut data as *mut u64 as *mut _),
      Some(&mut size),
    )
  };
  (res == ERROR_SUCCESS).then_some(data)
}

fn open_key(parent: HKEY, path: &HSTRING) -> Option<HKEY> {
  let mut key = HKEY::default();
  let res = unsafe { RegOpenKeyExW(parent, PCWSTR(path.as_ptr()), Some(0), KEY_READ, &mut key) };
  (res == ERROR_SUCCESS).then_some(key)
}

fn close_key(key: HKEY) {
  unsafe {
    let _ = RegCloseKey(key);
  }
}

fn subkey_names(key: HKEY) -> Vec<String> {
  let mut names = Vec::new();
  // Key names are at most 255 characters.
  let mut buf = [0u16; 256];
  for index in 0.. {
    let mut len = buf.len() as u32;
    let res = unsafe { RegEnumKeyExW(key, index, Some(PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None) };
    if res != ERROR_SUCCESS {
      break; // ERROR_NO_MORE_ITEMS, or a key we can't read: stop either way.
    }
    if let Some(name) = buf.get(..len as usize) {
      names.push(String::from_utf16_lossy(name));
    }
  }
  names
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn recording_into_a_muted_or_missing_mic_is_flagged() {
    assert_eq!(silenced(true, Some((0.8, true))), Some(Silenced::Muted));
    assert_eq!(silenced(true, Some((0.0, false))), Some(Silenced::ZeroVolume));
    assert_eq!(silenced(false, None), Some(Silenced::NoMicrophone));
    assert_eq!(silenced(true, Some((0.5, false))), None);
    assert_eq!(silenced(true, None), None, "unreadable level: no false alarm");
  }

  #[test]
  fn display_names_are_short() {
    assert_eq!(display_name(r"C:#Program Files#Discord#Discord.exe", true), "Discord.exe");
    assert_eq!(display_name("Microsoft.WindowsSoundRecorder_8wekyb3d8bbwe", false), "WindowsSoundRecorder");
    assert_eq!(display_name("NoDots_hash", false), "NoDots");
    assert_eq!(display_name("", false), "");
  }
}
