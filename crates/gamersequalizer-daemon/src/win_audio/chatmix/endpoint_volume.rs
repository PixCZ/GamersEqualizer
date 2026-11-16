//! Endpoint volumes, mutes and peaks over COM, with the activated interfaces cached per thread.

use super::*;

/// `QueryHardwareSupport` bit: the device applies the volume itself (endpointvolume.h; the
/// windows crate doesn't export it).
const ENDPOINT_HARDWARE_SUPPORT_VOLUME: u32 = 0x1;

thread_local! {
  /// Activated endpoint interfaces per device id. Creating the enumerator, looking the device
  /// up and activating it for every read was ~200 RPCs per second into the audio service from
  /// the window alone. Per thread: COM objects stay in the apartment that made them. An entry
  /// is dropped on any error (device removed, audio service restarted) and made fresh next time.
  static ENDPOINT_VOLUMES: RefCell<HashMap<String, IAudioEndpointVolume>> = RefCell::new(HashMap::new());
  static ENDPOINT_METERS: RefCell<HashMap<String, IAudioMeterInformation>> = RefCell::new(HashMap::new());
  static SESSION_MANAGERS: RefCell<HashMap<String, IAudioSessionManager2>> = RefCell::new(HashMap::new());
}

/// Bound for the caches (a PC has a handful of endpoints; ids of unplugged ones pile up).
const MAX_CACHED_ENDPOINTS: usize = 64;

/// Runs `f` on the cached `I` of `device_id`, activating it first if needed.
fn with_endpoint<I: Interface + Clone, T>(
  cache: &'static LocalKey<RefCell<HashMap<String, I>>>,
  device_id: &str,
  f: impl FnOnce(&I) -> windows::core::Result<T>,
) -> Option<T> {
  let cached = cache.with_borrow(|c| c.get(device_id).cloned());
  let iface = match cached {
    Some(i) => i,
    None => {
      // SAFETY: plain COM activation; the interface is reference counted.
      let i = unsafe { activate_endpoint::<I>(device_id)? };
      cache.with_borrow_mut(|c| {
        if c.len() >= MAX_CACHED_ENDPOINTS {
          c.clear();
        }
        c.insert(device_id.to_string(), i.clone());
      });
      i
    }
  };
  match f(&iface) {
    Ok(v) => Some(v),
    Err(_) => {
      cache.with_borrow_mut(|c| c.remove(device_id));
      None
    }
  }
}

unsafe fn activate_endpoint<I: Interface>(device_id: &str) -> Option<I> {
  // SAFETY: COM is initialized on this thread first (an already initialized thread keeps its apartment); every
  // interface is created in this block and released when dropped.
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
    let device: IMMDevice = enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())).ok()?;
    device.Activate::<I>(CLSCTX_ALL, None).ok()
  }
}

/// Another process has a running stream on `device_id` (a microphone: an app records from this
/// very device; an output: an app has a stream open, even a silent one). The audio engine then
/// runs the device's effects. None = could not ask.
pub fn endpoint_used_by_others(device_id: &str) -> Option<bool> {
  // SAFETY: no arguments, cannot fail.
  let own_pid = unsafe { GetCurrentProcessId() };
  // SAFETY: COM calls on live interfaces.
  with_endpoint(&SESSION_MANAGERS, device_id, |manager: &IAudioSessionManager2| unsafe {
    let sessions = manager.GetSessionEnumerator()?;
    for i in 0..sessions.GetCount()? {
      let Ok(control) = sessions.GetSession(i) else { continue };
      if control.GetState().ok() != Some(AudioSessionStateActive) {
        continue;
      }
      // Our own streams (speaker loopback) don't count.
      let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
      if control2.GetProcessId().is_ok_and(|pid| pid != own_pid) {
        return Ok(true);
      }
    }
    Ok(false)
  })
}

pub fn get_endpoint_volume(device_id: &str) -> Option<(f32, bool)> {
  // SAFETY: COM calls on a live interface.
  with_endpoint(&ENDPOINT_VOLUMES, device_id, |v: &IAudioEndpointVolume| unsafe {
    Ok((v.GetMasterVolumeLevelScalar()?, v.GetMute()?.as_bool()))
  })
}

/// Windows mic/speaker volume of an endpoint in dB (the gain it applies), and whether the device
/// does it in hardware (only then does a lower volume prevent the converter from clipping).
pub fn get_endpoint_volume_db(device_id: &str) -> Option<(f32, bool)> {
  // SAFETY: COM calls on a live interface.
  with_endpoint(&ENDPOINT_VOLUMES, device_id, |v: &IAudioEndpointVolume| unsafe {
    let db = v.GetMasterVolumeLevel()?;
    let hardware = v.QueryHardwareSupport().is_ok_and(|mask| mask & ENDPOINT_HARDWARE_SUPPORT_VOLUME != 0);
    Ok((db, hardware))
  })
}

pub fn set_endpoint_volume(device_id: &str, level: f32) {
  // SAFETY: COM call on a live interface.
  with_endpoint(&ENDPOINT_VOLUMES, device_id, |v: &IAudioEndpointVolume| unsafe {
    v.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null())
  });
}

pub fn set_endpoint_mute(device_id: &str, muted: bool) {
  // SAFETY: COM call on a live interface.
  with_endpoint(&ENDPOINT_VOLUMES, device_id, |v: &IAudioEndpointVolume| unsafe { v.SetMute(muted, std::ptr::null()) });
}

pub fn get_system_master_volume() -> Option<(f32, bool)> {
  // SAFETY: COM is initialized on this thread first (an already initialized thread keeps its apartment); every
  // interface is created in this block and released when dropped.
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
    let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
    let endpoint_vol: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
    let level = endpoint_vol.GetMasterVolumeLevelScalar().ok()?;
    let muted = endpoint_vol.GetMute().ok()?;
    Some((level, muted.as_bool()))
  }
}

pub fn set_system_master_volume(level: f32) {
  // SAFETY: COM is initialized on this thread first (an already initialized thread keeps its apartment); every
  // interface is created in this block and released when dropped.
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    if let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
      && let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)
      && let Ok(endpoint_vol) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
    {
      let _ = endpoint_vol.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null());
    }
  }
}

pub fn set_system_master_mute(muted: bool) {
  // SAFETY: COM is initialized on this thread first (an already initialized thread keeps its apartment); every
  // interface is created in this block and released when dropped.
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    if let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
      && let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)
      && let Ok(endpoint_vol) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
    {
      let _ = endpoint_vol.SetMute(muted, std::ptr::null());
    }
  }
}

pub fn get_system_mic_volume() -> Option<(f32, bool)> {
  // SAFETY: COM is initialized on this thread first (an already initialized thread keeps its apartment); every
  // interface is created in this block and released when dropped.
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
    let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole).ok()?;
    let endpoint_vol: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
    let level = endpoint_vol.GetMasterVolumeLevelScalar().ok()?;
    let muted = endpoint_vol.GetMute().ok()?;
    Some((level, muted.as_bool()))
  }
}

pub fn get_system_mic_peak() -> Option<f32> {
  // SAFETY: COM is initialized on this thread first (an already initialized thread keeps its apartment); every
  // interface is created in this block and released when dropped.
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
    let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole).ok()?;
    let meter: IAudioMeterInformation = device.Activate(CLSCTX_ALL, None).ok()?;
    meter.GetPeakValue().ok()
  }
}

pub fn get_endpoint_peak(device_id: &str) -> Option<f32> {
  // SAFETY: COM call on a live interface.
  with_endpoint(&ENDPOINT_METERS, device_id, |m: &IAudioMeterInformation| unsafe { m.GetPeakValue() })
}
