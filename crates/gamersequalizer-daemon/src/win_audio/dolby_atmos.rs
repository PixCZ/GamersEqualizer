use log::info;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Media::Audio::{
  eConsole, eRender, IMMDevice, IMMDeviceEnumerator, ISpatialAudioClient, ISpatialAudioObjectRenderStream,
  MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

/// Spatial sound on the default output, as Windows reports it.
///
/// Shortcut: Windows does not say *which* renderer is active (Dolby Atmos, DTS:X, Windows
/// Sonic); only an undocumented endpoint property would. Upgrade if the UI needs the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpatialAudioStatus {
  /// A spatial renderer is on: apps can render spatial objects, which bypass per-stream (SFX)
  /// effects and their session meter (measured with Dolby Atmos, Milestone 19).
  Active,
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

pub struct DolbyAtmosGuard {
  last_status: SpatialAudioStatus,
}

impl DolbyAtmosGuard {
  pub fn new() -> Self {
    Self { last_status: SpatialAudioStatus::Disabled }
  }

  /// Check active spatial audio status on default render endpoint
  pub fn check_spatial_status(&mut self) -> SpatialAudioStatus {
    let status = unsafe { self.query_active_endpoint() };
    if status != self.last_status {
      info!("[DolbyAtmosGuard] Spatial audio status transition: {:?} -> {:?}", self.last_status, status);
      self.last_status = status;
    }
    status
  }

  unsafe fn query_active_endpoint(&self) -> SpatialAudioStatus {
    let enumerator: IMMDeviceEnumerator = match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
      Ok(e) => e,
      Err(_) => return SpatialAudioStatus::Unsupported,
    };
    match enumerator.GetDefaultAudioEndpoint(eRender, eConsole) {
      Ok(device) => device_status(&device),
      Err(_) => SpatialAudioStatus::Unsupported,
    }
  }
}

/// Spatial sound on one output (full endpoint id), for the tray's setup check.
pub fn spatial_status(device_id: &str) -> SpatialAudioStatus {
  let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
  // SAFETY: COM is initialized on the calling thread; `wide` is NUL-terminated and outlives the call.
  unsafe {
    let enumerator: IMMDeviceEnumerator = match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
      Ok(e) => e,
      Err(_) => return SpatialAudioStatus::Unsupported,
    };
    match enumerator.GetDevice(PCWSTR(wide.as_ptr())) {
      Ok(device) => device_status(&device),
      Err(_) => SpatialAudioStatus::Unsupported,
    }
  }
}

/// SAFETY: COM must be initialized on the calling thread.
unsafe fn device_status(device: &IMMDevice) -> SpatialAudioStatus {
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
