//! The MMDevice enumerator and the endpoint lookups built on it: the one place that creates the
//! enumerator, opens a device by id and frees the id text Windows allocates. COM must already be
//! initialized on the calling thread (each thread does that itself); without it these return
//! the COM error. They do not initialize it here: the window's thread must stay free for the
//! apartment its own libraries pick.

use windows::Win32::Media::Audio::{
  EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows_core::{PCWSTR, Result};

pub fn enumerator() -> Result<IMMDeviceEnumerator> {
  // SAFETY: no pointers; the enumerator is returned owned and released when dropped.
  unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// The endpoint with this full id (`{0.0.1.00000000}.{guid}`).
pub fn device(id: &str) -> Result<IMMDevice> {
  let enumerator = enumerator()?;
  let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
  // SAFETY: `wide` is NUL-terminated and outlives the call.
  unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }
}

pub fn default_device(flow: EDataFlow, role: ERole) -> Result<IMMDevice> {
  let enumerator = enumerator()?;
  // SAFETY: a plain call on the live enumerator.
  unsafe { enumerator.GetDefaultAudioEndpoint(flow, role) }
}

/// The device's id; the text Windows allocates for it is freed here on every path.
pub fn device_id(device: &IMMDevice) -> Option<String> {
  // SAFETY: `device` is a live interface; the string GetId returns is read, then freed once.
  unsafe {
    let raw = device.GetId().ok()?;
    let id = raw.to_string().ok();
    CoTaskMemFree(Some(raw.as_ptr() as *const _));
    id
  }
}

/// Id of the current default endpoint for `flow` / `role`.
pub fn default_device_id(flow: EDataFlow, role: ERole) -> Option<String> {
  device_id(&default_device(flow, role).ok()?)
}

/// Id of the output the echo reference is taken from: the explicit one, else the Windows default.
pub fn resolve_render_id(explicit: Option<&str>) -> Option<String> {
  if let Some(id) = explicit {
    return Some(id.to_string());
  }
  default_device_id(eRender, eConsole)
}

/// Id of the Windows default microphone (the endpoint the APO most likely runs on).
pub fn resolve_default_capture_id() -> Option<String> {
  default_device_id(eCapture, eConsole)
}
