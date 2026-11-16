use log::info;
use serde::{Deserialize, Serialize};
use windows::Win32::Media::Audio::{
  IMMDevice, ISpatialAudioClient, ISpatialAudioObjectRenderStream, eConsole, eRender,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_BINARY, RegGetValueW};
use windows::core::{Interface, PCWSTR};

use super::apo_install::endpoint_guid_of;

// Which spatial format runs on an output is not in any public API; Windows keeps it in the
// endpoint's property store (mirrored in the registry, readable without admin rights). Layout
// read from Windows 11 26300 with Dolby Atmos, DTS and Windows Sonic installed. If Windows
// changes it, `active_format` returns None and the UI falls back to "Active".
const RENDER_KEY: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\MMDevices\\Audio\\Render";
/// The format that runs now: renderer id at `ACTIVE_ID_AT`; a short value when it is off.
const ACTIVE_FORMAT_VALUE: &str = "{fd8a7b27-0b18-4025-ab1c-bdd6b32e1604},2";
const ACTIVE_ID_AT: usize = 12;
/// Formats offered on the output, one value per property id: display name (UTF-16, in the
/// Windows language) at `OFFERED_NAME_AT`, renderer id at `OFFERED_ID_AT`. Every installed
/// renderer (Dolby, DTS, Windows Sonic, future ones) registers itself here.
const OFFERED_FMTID: &str = "{a45429a4-aa63-4480-b7f8-3f2552daee93}";
const OFFERED_NAME_AT: usize = 8;
const OFFERED_NAME_BYTES: usize = 0x200;
const OFFERED_ID_AT: usize = 0x308;
/// Property ids scanned for offered formats (a PC has a handful).
const MAX_OFFERED: u32 = 32;

/// Spatial sound on the default output, as Windows reports it (which format: `active_format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpatialAudioStatus {
  /// A spatial renderer is on: apps can render spatial objects, which bypass per-stream (SFX)
  /// effects and their session meter (measured with Dolby Atmos, Milestone 19).
  Active,
  #[default]
  Disabled,
  Unsupported,
}

impl SpatialAudioStatus {
  pub fn is_active(&self) -> bool {
    matches!(self, SpatialAudioStatus::Active)
  }

  /// English UI label (a translation key, see `i18n::tr`).
  pub fn label(&self) -> &'static str {
    use crate::i18n::tk;
    match self {
      SpatialAudioStatus::Active => tk("Active"),
      SpatialAudioStatus::Disabled => tk("Off"),
      SpatialAudioStatus::Unsupported => tk("Not supported"),
    }
  }
}

/// One spatial sound format (Dolby Atmos for Headphones, Windows Sonic, DTS Headphone:X, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpatialFormat {
  /// Renderer id (GUID, lowercase); stable across languages and renames.
  pub id: String,
  /// Display name as Windows shows it; empty when the offered list does not have it.
  pub name: String,
}

impl SpatialFormat {
  /// Name for the UI; `fallback` when Windows gave none.
  pub fn label<'a>(&'a self, fallback: &'a str) -> &'a str {
    if self.name.is_empty() { fallback } else { &self.name }
  }
}

#[derive(Default)]
pub struct DolbyAtmosGuard {
  last_status: SpatialAudioStatus,
}

impl DolbyAtmosGuard {
  /// Check active spatial audio status on default render endpoint
  pub fn check_spatial_status(&mut self) -> SpatialAudioStatus {
    // SAFETY: the service and window threads initialize COM at start; on a thread without it CoCreateInstance fails
    // and the status reads Unsupported.
    let status = unsafe { self.query_active_endpoint() };
    if status != self.last_status {
      info!("[DolbyAtmosGuard] Spatial audio status transition: {:?} -> {:?}", self.last_status, status);
      self.last_status = status;
    }
    status
  }

  /// # Safety
  /// COM must be initialized on the calling thread.
  unsafe fn query_active_endpoint(&self) -> SpatialAudioStatus {
    // SAFETY: the contract above; the interfaces are created here and released when dropped.
    unsafe {
      match super::mmdevice::default_device(eRender, eConsole) {
        Ok(device) => device_status(&device),
        Err(_) => SpatialAudioStatus::Unsupported,
      }
    }
  }
}

/// Spatial sound on one output (full endpoint id), for the tray's setup check.
pub fn spatial_status(device_id: &str) -> SpatialAudioStatus {
  // SAFETY: COM is initialized on the calling thread; the device is live for the call.
  unsafe {
    match super::mmdevice::device(device_id) {
      Ok(device) => device_status(&device),
      Err(_) => SpatialAudioStatus::Unsupported,
    }
  }
}

/// # Safety
/// COM must be initialized on the calling thread.
unsafe fn device_status(device: &IMMDevice) -> SpatialAudioStatus {
  // SAFETY: the contract above; `device` is live and the spatial client is created here; the IID is a static.
  unsafe {
    let spatial_client: ISpatialAudioClient = match device.Activate(CLSCTX_ALL, None) {
      Ok(c) => c,
      Err(_) => return SpatialAudioStatus::Unsupported,
    };

    // The first argument is the IID of the stream interface to open, not a format GUID
    // (passing format GUIDs here always failed, so Atmos read as "off").
    match spatial_client.IsSpatialAudioStreamAvailable(&ISpatialAudioObjectRenderStream::IID, None) {
      Ok(()) => SpatialAudioStatus::Active,
      Err(_) => SpatialAudioStatus::Disabled,
    }
  }
}

/// The spatial format running on one output (full endpoint id); None when off or unknown.
pub fn active_format(device_id: &str) -> Option<SpatialFormat> {
  let guid = endpoint_guid_of(device_id)?;
  let key = format!("{RENDER_KEY}\\{guid}\\Properties");
  let id = guid_at(&read_binary_value(&key, ACTIVE_FORMAT_VALUE)?, ACTIVE_ID_AT)?;
  let name = (1..=MAX_OFFERED)
    .filter_map(|pid| read_binary_value(&key, &format!("{OFFERED_FMTID},{pid}")))
    .filter_map(|blob| parse_offered(&blob))
    .find(|offered| offered.id == id)
    .map(|offered| offered.name)
    .unwrap_or_default();
  Some(SpatialFormat { id, name })
}

/// One entry of the offered list.
fn parse_offered(blob: &[u8]) -> Option<SpatialFormat> {
  let id = guid_at(blob, OFFERED_ID_AT)?;
  let name_bytes = blob.get(OFFERED_NAME_AT..OFFERED_NAME_AT + OFFERED_NAME_BYTES)?;
  let units: Vec<u16> =
    name_bytes.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).take_while(|&u| u != 0).collect();
  Some(SpatialFormat { id, name: String::from_utf16_lossy(&units).trim().to_string() })
}

/// GUID stored little-endian at `at`, as `xxxxxxxx-xxxx-...` lowercase; None if missing or zero.
fn guid_at(blob: &[u8], at: usize) -> Option<String> {
  let b: [u8; 16] = blob.get(at..at + 16)?.try_into().ok()?;
  if b.iter().all(|&x| x == 0) {
    return None;
  }
  let [a0, a1, a2, a3, b0, b1, c0, c1, d @ ..] = b;
  let d: String = d.iter().map(|x| format!("{x:02x}")).collect();
  Some(format!(
    "{:08x}-{:04x}-{:04x}-{}-{}",
    u32::from_le_bytes([a0, a1, a2, a3]),
    u16::from_le_bytes([b0, b1]),
    u16::from_le_bytes([c0, c1]),
    d.get(..4)?,
    d.get(4..)?
  ))
}

fn read_binary_value(key: &str, value: &str) -> Option<Vec<u8>> {
  let key_w: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
  let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
  let mut size = 0u32;
  // SAFETY: first call only queries the size; the second writes into a buffer of that size.
  unsafe {
    let key_p = PCWSTR(key_w.as_ptr());
    let value_p = PCWSTR(value_w.as_ptr());
    RegGetValueW(HKEY_LOCAL_MACHINE, key_p, value_p, RRF_RT_REG_BINARY, None, None, Some(&mut size)).ok().ok()?;
    let mut buf = vec![0u8; size as usize];
    RegGetValueW(
      HKEY_LOCAL_MACHINE,
      key_p,
      value_p,
      RRF_RT_REG_BINARY,
      None,
      Some(buf.as_mut_ptr().cast()),
      Some(&mut size),
    )
    .ok()
    .ok()?;
    buf.truncate(size as usize);
    Some(buf)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Dolby Atmos for Headphones, as stored in the registry.
  const DOLBY_ID: [u8; 16] =
    [0x38, 0xac, 0x59, 0x14, 0x75, 0x38, 0xbf, 0x49, 0xbb, 0x59, 0x0f, 0xe8, 0x0f, 0x4d, 0x39, 0x5d];

  #[test]
  fn active_format_id_is_read_and_off_is_none() {
    let mut on = vec![0x41, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0x5a];
    on.extend_from_slice(&DOLBY_ID);
    on.extend_from_slice(&[0xfe, 0xff, 0x0f, 0]);
    assert_eq!(guid_at(&on, ACTIVE_ID_AT).as_deref(), Some("1459ac38-3875-49bf-bb59-0fe80f4d395d"));
    let off = [0x41, 0, 0, 0, 1, 0, 0, 0, 0];
    assert_eq!(guid_at(&off, ACTIVE_ID_AT), None);
    assert_eq!(guid_at(&[0u8; 40], ACTIVE_ID_AT), None, "zero id = nothing selected");
  }

  #[test]
  fn offered_entry_gives_name_and_id() {
    let mut blob = vec![0u8; 842];
    blob[..8].copy_from_slice(&[0x41, 0, 0, 0, 1, 0, 0, 0]);
    for (i, u) in "DTS Headphone:X".encode_utf16().enumerate() {
      blob[OFFERED_NAME_AT + 2 * i..OFFERED_NAME_AT + 2 * i + 2].copy_from_slice(&u.to_le_bytes());
    }
    blob[OFFERED_ID_AT..OFFERED_ID_AT + 16].copy_from_slice(&DOLBY_ID);
    let f = parse_offered(&blob).unwrap_or_else(|| SpatialFormat { id: String::new(), name: String::new() });
    assert_eq!(f.name, "DTS Headphone:X");
    assert_eq!(f.id, "1459ac38-3875-49bf-bb59-0fe80f4d395d");
    assert_eq!(parse_offered(&blob[..100]), None, "truncated value");
  }
}
