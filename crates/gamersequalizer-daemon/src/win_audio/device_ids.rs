//! Keeps settings bound to the same physical device when its Windows endpoint id changes:
//! another USB port, a reinstalled Windows, settings imported from another PC. Windows gives
//! such a device a new id, but its name stays ("Headphones (EPOS GSA 70)").
//!
//! Every endpoint seen is remembered in `devices.json` (id -> name). When a remembered id is
//! gone and exactly one connected device has the same name, the old id is replaced by the new
//! one in the settings files that store device ids.

use super::endpoint_lock::AudioEndpointInfo;
use crate::output_eq::settings::write_atomic;
use log::{info, warn};
use std::collections::BTreeMap;
use std::path::Path;

pub const KNOWN_FILE: &str = "devices.json";
/// Settings files (in the profiles folder) that store endpoint ids.
pub const FILES_WITH_IDS: [&str; 3] = ["output_eq.json", "audio_settings.json", "sidetone.json"];

/// Remembered endpoints: id -> name.
pub type KnownDevices = BTreeMap<String, String>;

pub fn load_known(profiles_dir: &Path) -> KnownDevices {
  let path = profiles_dir.join(KNOWN_FILE);
  let Ok(text) = std::fs::read_to_string(&path) else {
    return KnownDevices::new();
  };
  serde_json::from_str(&text).unwrap_or_else(|e| {
    warn!("[Devices] {} is corrupt ({e}), starting a new list", path.display());
    KnownDevices::new()
  })
}

pub fn save_known(profiles_dir: &Path, known: &KnownDevices) {
  let path = profiles_dir.join(KNOWN_FILE);
  let saved = serde_json::to_string_pretty(known).map_err(std::io::Error::other).and_then(|t| write_atomic(&path, &t));
  if let Err(e) = saved {
    warn!("[Devices] could not save {}: {e}", path.display());
  }
}

/// Remembers the connected devices and moves settings from vanished ids to the same device's
/// new id. Returns the (old, new) pairs that were applied.
pub fn heal(profiles_dir: &Path, connected: &[AudioEndpointInfo]) -> Vec<(String, String)> {
  let mut known = load_known(profiles_dir);
  let moves = remap(&known, connected);
  let mut changed = false;
  for d in connected {
    if known.get(&d.id) != Some(&d.name) {
      known.insert(d.id.clone(), d.name.clone());
      changed = true;
    }
  }
  if changed {
    save_known(profiles_dir, &known);
  }
  if moves.is_empty() {
    return moves;
  }
  for file in FILES_WITH_IDS {
    let path = profiles_dir.join(file);
    let Ok(text) = std::fs::read_to_string(&path) else { continue };
    let new_text = replace_ids(&text, &moves);
    if new_text != text {
      match write_atomic(&path, &new_text) {
        Ok(()) => info!("[Devices] {file}: device ids updated"),
        Err(e) => warn!("[Devices] could not update {}: {e}", path.display()),
      }
    }
  }
  for (old, new) in &moves {
    info!("[Devices] {:?} has a new id: {old} -> {new}", known.get(new).map_or("?", String::as_str));
  }
  moves
}

/// (old id, new id) for every remembered id that is not connected while exactly one connected
/// device carries its name. Two identical devices connected at once stay ambiguous: no move.
pub fn remap(known: &KnownDevices, connected: &[AudioEndpointInfo]) -> Vec<(String, String)> {
  known
    .iter()
    .filter(|(id, _)| !connected.iter().any(|d| d.id.eq_ignore_ascii_case(id)))
    .filter_map(|(old, name)| {
      let wanted = normalized(name);
      let mut same = connected.iter().filter(|d| normalized(&d.name) == wanted);
      match (same.next(), same.next()) {
        (Some(d), None) if !wanted.is_empty() => Some((old.clone(), d.id.clone())),
        _ => None,
      }
    })
    .collect()
}

/// Replaces each old id, as a whole JSON string, by its new id.
pub fn replace_ids(text: &str, moves: &[(String, String)]) -> String {
  moves.iter().fold(text.to_string(), |t, (old, new)| t.replace(&format!("\"{old}\""), &format!("\"{new}\"")))
}

/// Name without the number Windows adds to a second identical device ("Headphones (2- EPOS)"),
/// trimmed and lowercase. shortcut: a device the user renamed in Windows no longer matches;
/// add the USB vendor/product id as a second key if testers hit that.
pub fn normalized(name: &str) -> String {
  let lower = name.trim().to_lowercase();
  let mut out = String::with_capacity(lower.len());
  let mut rest = lower.as_str();
  let mut at_start = true;
  while !rest.is_empty() {
    if at_start {
      let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
      if digits > 0 && rest.get(digits..).is_some_and(|r| r.starts_with("- ")) {
        rest = rest.get(digits + 2..).unwrap_or_default();
      }
    }
    let Some(c) = rest.chars().next() else { break };
    out.push(c);
    at_start = c == '(';
    rest = rest.get(c.len_utf8()..).unwrap_or_default();
  }
  out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
  use super::*;

  fn dev(id: &str, name: &str) -> AudioEndpointInfo {
    AudioEndpointInfo { id: id.into(), name: name.into(), is_default: false }
  }

  #[test]
  fn windows_numbering_is_ignored() {
    assert_eq!(normalized("Headphones (2- EPOS GSA 70)"), normalized("Headphones (EPOS GSA 70)"));
    assert_eq!(normalized("12- Speakers"), "speakers");
    assert_eq!(normalized("Mic (Realtek(R) Audio)"), "mic (realtek(r) audio)");
    assert_eq!(normalized("Audio 2-channel"), "audio 2-channel", "a number inside a word stays");
  }

  #[test]
  fn a_device_on_another_port_keeps_its_settings() {
    let known: KnownDevices = [("{old-epos}".to_string(), "Headphones (EPOS GSA 70)".to_string())].into();
    let now = [dev("{new-epos}", "Headphones (2- EPOS GSA 70)"), dev("{realtek}", "Speakers (Realtek(R) Audio)")];
    assert_eq!(remap(&known, &now), [("{old-epos}".to_string(), "{new-epos}".to_string())]);

    // Still connected: nothing moves. Two identical ones: ambiguous, nothing moves.
    assert!(remap(&known, &[dev("{old-epos}", "Headphones (EPOS GSA 70)")]).is_empty());
    let twins = [dev("{a}", "Headphones (EPOS GSA 70)"), dev("{b}", "Headphones (2- EPOS GSA 70)")];
    assert!(remap(&known, &twins).is_empty());
  }

  #[test]
  fn ids_are_replaced_only_as_whole_strings() {
    let text = r#"{"headphone_by_device":{"{0.0.0}.{abc}":"NTH-100"},"mic":"{0.0.0}.{abcd}"}"#;
    let moves = [("{0.0.0}.{abc}".to_string(), "{0.0.0}.{xyz}".to_string())];
    assert_eq!(
      replace_ids(text, &moves),
      r#"{"headphone_by_device":{"{0.0.0}.{xyz}":"NTH-100"},"mic":"{0.0.0}.{abcd}"}"#
    );
  }

  #[test]
  fn heal_rewrites_settings_and_remembers_devices() {
    let dir = std::env::temp_dir().join(format!("ge_device_ids_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("audio_settings.json"), r#"{"mic_device_id": "{old}"}"#).unwrap();

    assert!(heal(&dir, &[dev("{old}", "Mic (USB)")]).is_empty());
    assert_eq!(load_known(&dir).get("{old}").map(String::as_str), Some("Mic (USB)"));

    let moves = heal(&dir, &[dev("{new}", "Mic (USB)")]);
    assert_eq!(moves.len(), 1);
    assert_eq!(std::fs::read_to_string(dir.join("audio_settings.json")).unwrap(), r#"{"mic_device_id": "{new}"}"#);
    // Back on the first port: moves back.
    heal(&dir, &[dev("{old}", "Mic (USB)")]);
    assert_eq!(std::fs::read_to_string(dir.join("audio_settings.json")).unwrap(), r#"{"mic_device_id": "{old}"}"#);
    let _ = std::fs::remove_dir_all(&dir);
  }
}
