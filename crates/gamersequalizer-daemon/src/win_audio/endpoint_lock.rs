use log::{info, warn};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use windows::core::{implement, PCWSTR};
use windows::Win32::Media::Audio::{
  eCapture, eConsole, eRender, EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient,
  IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

use super::policy_config::IPolicyConfig;

pub const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY =
  PROPERTYKEY { fmtid: windows::core::GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0), pid: 14 };

#[derive(Debug, Clone)]
pub struct AudioEndpointInfo {
  pub id: String,
  pub name: String,
  pub is_default: bool,
}

pub struct EndpointLockState {
  pub locked_device_id: Mutex<Option<String>>,
  pub lock_enabled: AtomicBool,
  pub hijack_counter: AtomicU32,
  pub current_default_device_id: Mutex<Option<String>>,
  pub device_changed_flag: AtomicBool,
}

impl EndpointLockState {
  pub fn new() -> Self {
    Self {
      locked_device_id: Mutex::new(None),
      lock_enabled: AtomicBool::new(false),
      hijack_counter: AtomicU32::new(0),
      current_default_device_id: Mutex::new(None),
      device_changed_flag: AtomicBool::new(false),
    }
  }
}

#[implement(IMMNotificationClient)]
pub struct EndpointNotificationHandler {
  state: Arc<EndpointLockState>,
}

impl EndpointNotificationHandler {
  pub fn new(state: Arc<EndpointLockState>) -> Self {
    Self { state }
  }
}

impl IMMNotificationClient_Impl for EndpointNotificationHandler_Impl {
  fn OnDeviceStateChanged(&self, _pwstrdeviceid: &PCWSTR, _dwnewstate: DEVICE_STATE) -> windows::core::Result<()> {
    self.state.device_changed_flag.store(true, Ordering::SeqCst);
    Ok(())
  }

  fn OnDeviceAdded(&self, _pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
    self.state.device_changed_flag.store(true, Ordering::SeqCst);
    Ok(())
  }

  fn OnDeviceRemoved(&self, _pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
    self.state.device_changed_flag.store(true, Ordering::SeqCst);
    Ok(())
  }

  fn OnDefaultDeviceChanged(
    &self,
    flow: EDataFlow,
    _role: ERole,
    pwstrdefaultdeviceid: &PCWSTR,
  ) -> windows::core::Result<()> {
    if flow != eRender {
      // Default microphone changed (here or in Windows settings): refresh the device list.
      // The output lock below is for render endpoints only.
      if flow == eCapture {
        self.state.device_changed_flag.store(true, Ordering::SeqCst);
      }
      return Ok(());
    }

    let current_id = unsafe {
      if pwstrdefaultdeviceid.is_null() {
        String::new()
      } else {
        pwstrdefaultdeviceid.to_string().unwrap_or_default()
      }
    };

    if !current_id.is_empty() {
      if let Ok(mut guard) = self.state.current_default_device_id.lock() {
        *guard = Some(current_id.clone());
      }
      self.state.device_changed_flag.store(true, Ordering::SeqCst);
    }

    if self.state.lock_enabled.load(Ordering::Relaxed) {
      let locked_opt = {
        let guard = match self.state.locked_device_id.lock() {
          Ok(g) => g,
          Err(poisoned) => poisoned.into_inner(),
        };
        guard.clone()
      };

      if let Some(locked_id) = locked_opt {
        if !current_id.is_empty() && current_id != locked_id {
          let count = self.state.hijack_counter.fetch_add(1, Ordering::SeqCst) + 1;
          warn!(
            "[EndpointLock] Audio output changed to: {}. Reverting to locked: {} (Attempt #{})",
            current_id, locked_id, count
          );

          // Force back to locked endpoint
          if let Ok(policy) = IPolicyConfig::new() {
            let wide: Vec<u16> = locked_id.encode_utf16().chain(std::iter::once(0)).collect();
            let pcwstr = PCWSTR(wide.as_ptr());
            let _ = policy.set_default_endpoint_all_roles(pcwstr);
          }
        }
      }
    }

    Ok(())
  }

  fn OnPropertyValueChanged(&self, _pwstrdeviceid: &PCWSTR, _key: &PROPERTYKEY) -> windows::core::Result<()> {
    Ok(())
  }
}

pub struct EndpointLockManager {
  state: Arc<EndpointLockState>,
  enumerator: IMMDeviceEnumerator,
  client: IMMNotificationClient,
}

impl EndpointLockManager {
  pub fn new(state: Arc<EndpointLockState>) -> windows_core::Result<Self> {
    unsafe {
      let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
      let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
      let client: IMMNotificationClient = EndpointNotificationHandler::new(Arc::clone(&state)).into();
      enumerator.RegisterEndpointNotificationCallback(&client)?;

      Ok(Self { state, enumerator, client })
    }
  }

  /// Poll if system devices or default endpoint changed
  pub fn poll_device_changed(&self) -> bool {
    self.state.device_changed_flag.swap(false, Ordering::SeqCst)
  }

  /// Set Windows system default audio endpoint
  pub fn set_as_default_device(&self, device_id: &str) -> windows_core::Result<()> {
    let policy = IPolicyConfig::new()?;
    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
    let pcwstr = PCWSTR(wide.as_ptr());
    policy.set_default_endpoint_all_roles(pcwstr)?;
    if let Ok(mut guard) = self.state.current_default_device_id.lock() {
      *guard = Some(device_id.to_string());
    }
    self.state.device_changed_flag.store(true, Ordering::SeqCst);
    info!("[EndpointLock] Set Windows default output endpoint to: {}", device_id);
    Ok(())
  }

  /// Set Windows communications default endpoint (for Discord, Calls, TeamSpeak)
  pub fn set_as_communications_device(&self, device_id: &str) -> windows_core::Result<()> {
    use windows::Win32::Media::Audio::eCommunications;
    let policy = IPolicyConfig::new()?;
    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
    let pcwstr = PCWSTR(wide.as_ptr());
    policy.set_default_endpoint(pcwstr, eCommunications)?;
    info!("[EndpointLock] Set Windows communications default endpoint to: {}", device_id);
    Ok(())
  }

  /// Set Windows default recording device (microphone)
  pub fn set_as_default_recording_device(&self, device_id: &str) -> windows_core::Result<()> {
    let policy = IPolicyConfig::new()?;
    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
    let pcwstr = PCWSTR(wide.as_ptr());
    policy.set_default_endpoint_all_roles(pcwstr)?;
    info!("[EndpointLock] Set Windows default recording endpoint to: {}", device_id);
    Ok(())
  }

  /// Enumerate active capture recording devices (microphones)
  pub fn enumerate_capture_devices(&self) -> Vec<AudioEndpointInfo> {
    let mut list = Vec::new();
    unsafe {
      let default_id = self.get_default_capture_id().unwrap_or_default();
      if let Ok(collection) = self.enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) {
        if let Ok(count) = collection.GetCount() {
          for i in 0..count {
            if let Ok(dev) = collection.Item(i) {
              if let Ok(info) = get_endpoint_info(&dev, &default_id) {
                list.push(info);
              }
            }
          }
        }
      }
    }
    list
  }

  pub fn get_default_capture_id(&self) -> Option<String> {
    unsafe {
      if let Ok(device) = self.enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) {
        if let Ok(pwstr) = device.GetId() {
          let s = pwstr.to_string().ok();
          let _ = windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.as_ptr() as *const _));
          return s;
        }
      }
      None
    }
  }

  /// Enumerate active output playback devices
  pub fn enumerate_playback_devices(&self) -> Vec<AudioEndpointInfo> {
    let mut list = Vec::new();
    unsafe {
      let default_id = self.get_default_output_id().unwrap_or_default();
      if let Ok(collection) = self.enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
        if let Ok(count) = collection.GetCount() {
          for i in 0..count {
            if let Ok(dev) = collection.Item(i) {
              if let Ok(info) = get_endpoint_info(&dev, &default_id) {
                list.push(info);
              }
            }
          }
        }
      }
    }
    list
  }

  pub fn get_default_output_id(&self) -> Option<String> {
    unsafe {
      if let Ok(device) = self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) {
        if let Ok(pwstr) = device.GetId() {
          let s = pwstr.to_string().ok();
          let _ = windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.as_ptr() as *const _));
          return s;
        }
      }
      None
    }
  }

  pub fn get_default_communications_id(&self) -> Option<String> {
    unsafe {
      use windows::Win32::Media::Audio::eCommunications;
      if let Ok(device) = self.enumerator.GetDefaultAudioEndpoint(eRender, eCommunications) {
        if let Ok(pwstr) = device.GetId() {
          let s = pwstr.to_string().ok();
          let _ = windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.as_ptr() as *const _));
          return s;
        }
      }
      None
    }
  }

  /// Lock active endpoint
  pub fn lock_device(&self, device_id: String) {
    {
      let mut guard = match self.state.locked_device_id.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
      };
      *guard = Some(device_id.clone());
    }
    self.state.lock_enabled.store(true, Ordering::SeqCst);
    info!("[EndpointLock] Device locked: {}", device_id);

    // Ensure it is set as default right now
    let _ = self.set_as_default_device(&device_id);
  }

  /// Unlock endpoint
  pub fn unlock(&self) {
    self.state.lock_enabled.store(false, Ordering::SeqCst);
    info!("[EndpointLock] Device lock disabled.");
  }

  pub fn is_locked(&self) -> bool {
    self.state.lock_enabled.load(Ordering::Relaxed)
  }

  pub fn hijack_count(&self) -> u32 {
    self.state.hijack_counter.load(Ordering::Relaxed)
  }
}

impl Drop for EndpointLockManager {
  fn drop(&mut self) {
    unsafe {
      let _ = self.enumerator.UnregisterEndpointNotificationCallback(&self.client);
    }
  }
}

pub(crate) unsafe fn get_endpoint_info(
  device: &IMMDevice,
  default_id: &str,
) -> windows_core::Result<AudioEndpointInfo> {
  let pwstr = device.GetId()?;
  let id = pwstr.to_string()?;
  windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.as_ptr() as *const _));

  let mut name = "Unknown Device".to_string();
  if let Ok(props) = device.OpenPropertyStore(STGM_READ) {
    if let Ok(val) = props.GetValue(&PKEY_DEVICE_FRIENDLY_NAME) {
      if let Ok(pwstr_name) = windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc(&val) {
        if let Ok(s) = pwstr_name.to_string() {
          name = s;
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(pwstr_name.as_ptr() as *const _));
      }
    }
  }

  let is_default = id == default_id;

  Ok(AudioEndpointInfo { id, name, is_default })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_enumerate_devices() {
    let state = Arc::new(EndpointLockState::new());
    if let Ok(manager) = EndpointLockManager::new(state) {
      let devices = manager.enumerate_playback_devices();
      println!("Found {} playback devices:", devices.len());
      for d in devices {
        println!("  - [{}] {} (default: {})", d.id, d.name, d.is_default);
        // Query endpoint volume
        unsafe {
          let wide: Vec<u16> = d.id.encode_utf16().chain(std::iter::once(0)).collect();
          if let Ok(dev) = manager.enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())) {
            if let Ok(vol) = dev.Activate::<windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume>(
              windows::Win32::System::Com::CLSCTX_ALL,
              None,
            ) {
              let level = vol.GetMasterVolumeLevelScalar().unwrap_or(0.0);
              let muted = vol.GetMute().map(|m| m.as_bool()).unwrap_or(false);
              println!("      -> Volume: {:.1}%, Muted: {}", level * 100.0, muted);
            }
          }
        }
      }
      let def_id = manager.get_default_output_id();
      println!("Default output ID from get_default_output_id: {:?}", def_id);
    }
  }
}
