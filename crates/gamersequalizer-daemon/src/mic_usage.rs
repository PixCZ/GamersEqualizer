//! "Is any app recording from a microphone right now?", read the way the Windows privacy
//! indicator does it: every app that opened a mic has a key under ConsentStore\microphone, and
//! `LastUsedTimeStop == 0` (with a start time) means its capture stream is still open, even
//! while the mic is silent. Covers every microphone and works without our APO.

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
  RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_QWORD,
};

const MIC_STORE: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";
/// Desktop (non-Store) apps live one level deeper, one key per exe path with `\` written as `#`.
const NON_PACKAGED: &str = "NonPackaged";

/// True when some app other than this one keeps a microphone open.
pub fn microphone_in_use() -> bool {
  !recording_apps().is_empty()
}

/// Apps other than this one that keep a microphone open right now, as short display names
/// (`Discord.exe`, `WindowsSoundRecorder`), sorted, without duplicates.
pub fn recording_apps() -> Vec<String> {
  // Our own mic bridge opens the mic too; it is not "another app recording".
  let own = std::env::current_exe().ok().map(|p| p.to_string_lossy().replace('\\', "#").to_lowercase());
  let is_own = |name: &str| own.as_deref() == Some(name.to_lowercase().as_str());

  let store = HSTRING::from(MIC_STORE);
  let Some(root) = open_key(HKEY_CURRENT_USER, &store) else { return Vec::new() };
  let mut apps = Vec::new();
  for name in subkey_names(root) {
    if name == NON_PACKAGED {
      if let Some(np) = open_key(root, &HSTRING::from(NON_PACKAGED)) {
        for app in subkey_names(np) {
          if !is_own(&app) && app_in_use(np, &app) {
            apps.push(display_name(&app, true));
          }
        }
        close_key(np);
      }
    } else if app_in_use(root, &name) {
      apps.push(display_name(&name, false));
    }
  }
  close_key(root);
  apps.sort_by_key(|a| a.to_lowercase());
  apps.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
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

fn app_in_use(parent: HKEY, app: &str) -> bool {
  let key = HSTRING::from(app);
  let start = read_qword(parent, &key, "LastUsedTimeStart");
  let stop = read_qword(parent, &key, "LastUsedTimeStop");
  matches!((start, stop), (Some(s), Some(0)) if s != 0)
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
  let res = unsafe { RegOpenKeyExW(parent, PCWSTR(path.as_ptr()), 0, KEY_READ, &mut key) };
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
    let res = unsafe { RegEnumKeyExW(key, index, PWSTR(buf.as_mut_ptr()), &mut len, None, PWSTR::null(), None, None) };
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
  fn display_names_are_short() {
    assert_eq!(display_name(r"C:#Program Files#Discord#Discord.exe", true), "Discord.exe");
    assert_eq!(display_name("Microsoft.WindowsSoundRecorder_8wekyb3d8bbwe", false), "WindowsSoundRecorder");
    assert_eq!(display_name("NoDots_hash", false), "NoDots");
    assert_eq!(display_name("", false), "");
  }
}
