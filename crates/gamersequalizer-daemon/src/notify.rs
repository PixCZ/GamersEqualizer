//! Windows notifications (toasts in the Action Center) for setup problems found while the
//! window is closed: spatial sound switched off, a microphone muted while an app records, a new
//! sound card without our effect, ...
//!
//! An unpackaged app needs an AppUserModelID Windows knows: registered per user under
//! `HKCU\Software\Classes\AppUserModelId\GamersEqualizer` (display name only, no admin). It also
//! gives the app its own entry in Settings > System > Notifications, where the user can turn
//! these off. `unregister` (Uninstall from Windows) removes it.
//!
//! Known limit: no click handling (that needs a COM activator); the tray menu has the fixes.

use log::{info, warn};
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
  HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW,
  RegSetValueExW,
};
use windows::core::{HSTRING, PCWSTR, w};

const AUMID: &str = "GamersEqualizer";
const AUMID_KEY: PCWSTR = w!(r"Software\Classes\AppUserModelId\GamersEqualizer");
const DISPLAY_NAME: &str = "GamersEqualizer";

/// Shows one toast. Failures are logged only: a missing notification never stops the service.
pub fn show(title: &str, body: &str) {
  if let Err(e) = register().and_then(|()| post(title, body)) {
    warn!("[Notify] could not show \"{title}\": {e}");
  } else {
    info!("[Notify] shown: {title}");
  }
}

/// Removes the notification registration (Uninstall from Windows).
pub fn unregister() -> Result<(), String> {
  // SAFETY: constant NUL-terminated key name.
  let rc = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, AUMID_KEY) };
  if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
    Ok(())
  } else {
    Err(format!("could not remove the notification registration: {rc:?}"))
  }
}

/// Idempotent: (re)writes the display name, cheap enough to do before every toast.
fn register() -> Result<(), String> {
  let mut key = HKEY::default();
  let name: Vec<u16> = DISPLAY_NAME.encode_utf16().chain(std::iter::once(0)).collect();
  // SAFETY: the key is closed below; `name` outlives the call and is passed with its byte length.
  unsafe {
    let rc = RegCreateKeyExW(
      HKEY_CURRENT_USER,
      AUMID_KEY,
      Some(0),
      None,
      REG_OPTION_NON_VOLATILE,
      KEY_WRITE,
      None,
      &mut key,
      None,
    );
    if rc != ERROR_SUCCESS {
      return Err(format!("could not register for notifications: {rc:?}"));
    }
    let bytes = std::slice::from_raw_parts(name.as_ptr().cast::<u8>(), name.len() * 2);
    let rc = RegSetValueExW(key, w!("DisplayName"), Some(0), REG_SZ, Some(bytes));
    let _ = RegCloseKey(key);
    if rc != ERROR_SUCCESS {
      return Err(format!("could not register for notifications: {rc:?}"));
    }
  }
  Ok(())
}

fn post(title: &str, body: &str) -> Result<(), String> {
  let xml = toast_xml(title, body);
  let doc = XmlDocument::new().map_err(|e| e.to_string())?;
  doc.LoadXml(&HSTRING::from(xml)).map_err(|e| e.to_string())?;
  let toast = ToastNotification::CreateToastNotification(&doc).map_err(|e| e.to_string())?;
  let notifier =
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID)).map_err(|e| e.to_string())?;
  notifier.Show(&toast).map_err(|e| e.to_string())
}

fn toast_xml(title: &str, body: &str) -> String {
  format!(
    "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
    xml_escape(title),
    xml_escape(body)
  )
}

/// Device and app names come from the system and may contain `<`, `&` or quotes.
fn xml_escape(s: &str) -> String {
  s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn names_from_the_system_are_escaped() {
    let xml = toast_xml("Mic <muted>", "\"R&D\" headset");
    assert!(xml.contains("<text>Mic &lt;muted&gt;</text>"));
    assert!(xml.contains("<text>&quot;R&amp;D&quot; headset</text>"));
  }
}
