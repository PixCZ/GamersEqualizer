use super::app_channels::{self, AppChannel};
use super::endpoint_lock::AudioEndpointInfo;
use std::collections::HashSet;
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE};
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
  eCapture, eConsole, eRender, IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator,
  ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::ProcessStatus::K32GetProcessImageFileNameW;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_core::Interface;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioBus {
  Master,
  Game,
  Chat,
  Media,
  Mic,
}

#[derive(Debug, Clone)]
pub struct AudioSessionInfo {
  pub pid: u32,
  pub exe_name: String,
  pub volume: f32,
  pub is_muted: bool,
  pub bus: AudioBus,
}

/// Apps on the Chat channel (ChatMix volume, chat stream EQ). Lowercase exe names.
pub const CHAT_EXES: &[&str] =
  &["discord.exe", "telegram.exe", "slack.exe", "teams.exe", "skype.exe", "zoom.exe", "whatsapp.exe"];
/// Apps on the Media channel.
pub const MEDIA_EXES: &[&str] = &[
  "chrome.exe",
  "msedge.exe",
  "firefox.exe",
  "brave.exe",
  "opera.exe",
  "spotify.exe",
  "vlc.exe",
  "foobar2000.exe",
  "tidal.exe",
  "musicbee.exe",
];

pub struct ChatMixController {
  pub balance: f32, // -1.0 (100% Game, 0% Chat) .. 0.0 (Equal) .. +1.0 (0% Game, 100% Chat)
  pub master_volume: f32,
  pub game_volume: f32,
  pub chat_volume: f32,
  pub media_volume: f32,
  pub mic_volume: f32,
  pub master_muted: bool,
  pub game_muted: bool,
  pub chat_muted: bool,
  pub media_muted: bool,
  pub mic_muted: bool,

  // Dedicated hardware or virtual endpoints for channels
  pub master_device_id: Option<String>,
  pub game_device_id: Option<String>,
  pub chat_device_id: Option<String>,
  pub media_device_id: Option<String>,
  pub mic_device_id: Option<String>,
  pub mic_peak_level: f32,

  // Track last applied values to avoid redundant COM calls
  last_applied_master_vol: f32,
  last_applied_master_muted: bool,
  last_applied_game_vol: f32,
  last_applied_game_muted: bool,
  last_applied_chat_vol: f32,
  last_applied_chat_muted: bool,
  last_applied_media_vol: f32,
  last_applied_media_muted: bool,
  last_applied_mic_vol: f32,
  last_applied_mic_muted: bool,

  chat_executables: HashSet<String>,
  media_executables: HashSet<String>,
  assigned_game_executables: HashSet<String>,
  current_game_exe: Option<String>,
}

impl ChatMixController {
  pub fn new() -> Self {
    let chat_executables: HashSet<String> = CHAT_EXES.iter().map(|e| e.to_string()).collect();
    let media_executables: HashSet<String> = MEDIA_EXES.iter().map(|e| e.to_string()).collect();

    // Read real current master output volume & mute from Windows
    let (initial_master_vol, initial_master_muted) = get_system_master_volume().unwrap_or((1.0, false));

    // Read real current microphone volume & mute from Windows
    let (initial_mic_vol, initial_mic_muted) = get_system_mic_volume().unwrap_or((0.8, false));

    Self {
      balance: 0.0,
      master_volume: initial_master_vol,
      game_volume: 1.0,
      chat_volume: 1.0,
      media_volume: 1.0,
      mic_volume: initial_mic_vol,
      master_muted: initial_master_muted,
      game_muted: false,
      chat_muted: false,
      media_muted: false,
      mic_muted: initial_mic_muted,
      master_device_id: None,
      game_device_id: None,
      chat_device_id: None,
      media_device_id: None,
      mic_device_id: None,
      mic_peak_level: 0.0,
      last_applied_master_vol: initial_master_vol,
      last_applied_master_muted: initial_master_muted,
      last_applied_game_vol: 1.0,
      last_applied_game_muted: false,
      last_applied_chat_vol: 1.0,
      last_applied_chat_muted: false,
      last_applied_media_vol: 1.0,
      last_applied_media_muted: false,
      last_applied_mic_vol: initial_mic_vol,
      last_applied_mic_muted: initial_mic_muted,
      chat_executables,
      media_executables,
      assigned_game_executables: HashSet::new(),
      current_game_exe: None,
    }
  }

  pub fn find_physical_microphone() -> Option<String> {
    unsafe {
      let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
      let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
      let collection = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE).ok()?;
      let count = collection.GetCount().ok()?;
      for i in 0..count {
        if let Ok(dev) = collection.Item(i) {
          let store = dev.OpenPropertyStore(windows::Win32::System::Com::STGM_READ).ok();
          let name = store
            .and_then(|s| s.GetValue(&crate::win_audio::endpoint_lock::PKEY_DEVICE_FRIENDLY_NAME).ok())
            .map(|v| v.to_string().to_lowercase())
            .unwrap_or_default();
          if !name.contains("sonar")
            && !name.contains("virtual")
            && !name.contains("cable")
            && !name.contains("steam")
            && !name.contains("oculus")
          {
            if let Ok(id) = dev.GetId() {
              return Some(id.to_string().unwrap_or_default());
            }
          }
        }
      }
      if let Ok(dev) = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) {
        if let Ok(id) = dev.GetId() {
          return Some(id.to_string().unwrap_or_default());
        }
      }
      None
    }
  }

  /// Detect and bind specific endpoints for Master, Game, Chat, Media
  pub fn bind_endpoints(&mut self, devices: &[AudioEndpointInfo], default_id: Option<&str>) {
    // 1. Identify virtual channel endpoints
    self.game_device_id = devices
      .iter()
      .find(|d| {
        let n = d.name.to_lowercase();
        n.contains("game") || n.contains("gaming")
      })
      .map(|d| d.id.clone());

    self.chat_device_id = devices
      .iter()
      .find(|d| {
        let n = d.name.to_lowercase();
        n.contains("chat") || n.contains("cable input")
      })
      .map(|d| d.id.clone());

    self.media_device_id = devices
      .iter()
      .find(|d| {
        let n = d.name.to_lowercase();
        n.contains("media")
      })
      .map(|d| d.id.clone());

    // 2. Identify physical Master endpoint
    let default_is_virtual = if let Some(def_id) = default_id {
      devices
        .iter()
        .find(|d| d.id == def_id)
        .map(|d| {
          let n = d.name.to_lowercase();
          n.contains("sonar") || n.contains("virtual") || n.contains("cable") || n.contains("game")
        })
        .unwrap_or(false)
    } else {
      false
    };

    if default_is_virtual {
      // Pick first physical output device (non-virtual, non-sonar, e.g. Realtek)
      self.master_device_id = devices
        .iter()
        .find(|d| {
          let n = d.name.to_lowercase();
          !n.contains("sonar")
            && !n.contains("virtual")
            && !n.contains("cable")
            && !n.contains("steam")
            && !n.contains("oculus")
        })
        .map(|d| d.id.clone())
        .or_else(|| default_id.map(|s| s.to_string()));
    } else {
      self.master_device_id = default_id.map(|s| s.to_string());
    }

    // Initialize channel volumes directly from the resolved endpoints
    if let Some(ref id) = self.master_device_id {
      if let Some((v, m)) = get_endpoint_volume(id) {
        self.master_volume = v;
        self.master_muted = m;
        self.last_applied_master_vol = v;
        self.last_applied_master_muted = m;
      }
    } else if let Some((v, m)) = get_system_master_volume() {
      self.master_volume = v;
      self.master_muted = m;
      self.last_applied_master_vol = v;
      self.last_applied_master_muted = m;
    }

    if let Some(ref id) = self.game_device_id {
      if let Some((v, m)) = get_endpoint_volume(id) {
        self.game_volume = v;
        self.game_muted = m;
        self.last_applied_game_vol = v;
        self.last_applied_game_muted = m;
      }
    }

    if let Some(ref id) = self.chat_device_id {
      if let Some((v, m)) = get_endpoint_volume(id) {
        self.chat_volume = v;
        self.chat_muted = m;
        self.last_applied_chat_vol = v;
        self.last_applied_chat_muted = m;
      }
    }

    if let Some(ref id) = self.media_device_id {
      if let Some((v, m)) = get_endpoint_volume(id) {
        self.media_volume = v;
        self.media_muted = m;
        self.last_applied_media_vol = v;
        self.last_applied_media_muted = m;
      }
    }

    self.mic_device_id = Self::find_physical_microphone();
    let mic_vol_opt =
      if let Some(ref id) = self.mic_device_id { get_endpoint_volume(id) } else { get_system_mic_volume() };
    if let Some((v, m)) = mic_vol_opt {
      self.mic_volume = v;
      self.mic_muted = m;
      self.last_applied_mic_vol = v;
      self.last_applied_mic_muted = m;
    }
  }

  /// Sync live volume/mute changes that occurred externally in Windows
  pub fn sync_from_system(&mut self) -> bool {
    let mut changed = false;

    // 1. MASTER: Sync with physical master endpoint
    let master_vol_opt =
      if let Some(ref id) = self.master_device_id { get_endpoint_volume(id) } else { get_system_master_volume() };
    if let Some((sys_vol, sys_muted)) = master_vol_opt {
      if (sys_vol - self.last_applied_master_vol).abs() > 0.005 {
        self.master_volume = sys_vol;
        self.last_applied_master_vol = sys_vol;
        changed = true;
      }
      if sys_muted != self.last_applied_master_muted {
        self.master_muted = sys_muted;
        self.last_applied_master_muted = sys_muted;
        changed = true;
      }
    }

    // 2. GAME: Sync with dedicated Game endpoint if present
    if let Some(ref id) = self.game_device_id {
      if let Some((sys_vol, sys_muted)) = get_endpoint_volume(id) {
        if (sys_vol - self.last_applied_game_vol).abs() > 0.005 {
          self.game_volume = sys_vol;
          self.last_applied_game_vol = sys_vol;
          changed = true;
        }
        if sys_muted != self.last_applied_game_muted {
          self.game_muted = sys_muted;
          self.last_applied_game_muted = sys_muted;
          changed = true;
        }
      }
    }

    // 3. CHAT: Sync with dedicated Chat endpoint if present
    if let Some(ref id) = self.chat_device_id {
      if let Some((sys_vol, sys_muted)) = get_endpoint_volume(id) {
        if (sys_vol - self.last_applied_chat_vol).abs() > 0.005 {
          self.chat_volume = sys_vol;
          self.last_applied_chat_vol = sys_vol;
          changed = true;
        }
        if sys_muted != self.last_applied_chat_muted {
          self.chat_muted = sys_muted;
          self.last_applied_chat_muted = sys_muted;
          changed = true;
        }
      }
    }

    // 4. MEDIA: Sync with dedicated Media endpoint if present
    if let Some(ref id) = self.media_device_id {
      if let Some((sys_vol, sys_muted)) = get_endpoint_volume(id) {
        if (sys_vol - self.last_applied_media_vol).abs() > 0.005 {
          self.media_volume = sys_vol;
          self.last_applied_media_vol = sys_vol;
          changed = true;
        }
        if sys_muted != self.last_applied_media_muted {
          self.media_muted = sys_muted;
          self.last_applied_media_muted = sys_muted;
          changed = true;
        }
      }
    }

    // 5. MIC: Sync with Windows microphone (prefer physical endpoint)
    let mic_vol_opt =
      if let Some(ref id) = self.mic_device_id { get_endpoint_volume(id) } else { get_system_mic_volume() };
    if let Some((sys_mic_vol, sys_mic_muted)) = mic_vol_opt {
      if (sys_mic_vol - self.mic_volume.min(1.0)).abs() > 0.015 {
        self.mic_volume = sys_mic_vol;
        self.last_applied_mic_vol = sys_mic_vol;
        changed = true;
      }
      if sys_mic_muted != self.last_applied_mic_muted {
        self.mic_muted = sys_mic_muted;
        self.last_applied_mic_muted = sys_mic_muted;
        changed = true;
      }
    }

    // 6. MIC PEAK: Read instantaneous audio peak level from physical mic
    let peak_opt = if let Some(ref id) = self.mic_device_id { get_endpoint_peak(id) } else { get_system_mic_peak() };
    if let Some(peak) = peak_opt {
      let boost = if self.mic_volume > 1.0 { self.mic_volume } else { 1.0 };
      let effective_peak = (peak * boost).clamp(0.0, 1.0);
      if effective_peak >= self.mic_peak_level {
        self.mic_peak_level = effective_peak;
      } else {
        self.mic_peak_level = (self.mic_peak_level * 0.88).max(effective_peak);
      }
    }

    changed
  }

  pub fn set_current_game(&mut self, exe_name: Option<String>) {
    self.current_game_exe = exe_name.map(|s| s.to_ascii_lowercase());
  }

  pub fn add_game_executable(&mut self, exe_name: String) {
    self.assigned_game_executables.insert(exe_name.to_ascii_lowercase());
  }

  pub fn set_balance(&mut self, balance: f32) {
    self.balance = balance.clamp(-1.0, 1.0);
    self.apply_mix();
  }

  pub fn classify_exe(&self, exe: &str) -> AudioBus {
    match app_channels::get(exe) {
      Some(AppChannel::Chat) => return AudioBus::Chat,
      Some(AppChannel::Media) => return AudioBus::Media,
      Some(AppChannel::Game) => return AudioBus::Game,
      Some(AppChannel::Unknown) | None => {}
    }
    let lower = exe.to_ascii_lowercase();
    if self.chat_executables.contains(&lower) {
      return AudioBus::Chat;
    }
    if self.media_executables.contains(&lower) {
      return AudioBus::Media;
    }
    if let Some(ref current) = self.current_game_exe {
      if &lower == current {
        return AudioBus::Game;
      }
    }
    if self.assigned_game_executables.contains(&lower) {
      return AudioBus::Game;
    }
    AudioBus::Game
  }

  /// Calculate effective multipliers for Game and Chat
  pub fn get_multipliers(&self) -> (f32, f32) {
    let b = self.balance;
    let (game_scale, chat_scale) = if b <= 0.0 { (1.0, 1.0 + b) } else { (1.0 - b, 1.0) };

    let g = if self.game_muted || self.master_muted { 0.0 } else { (self.game_volume * game_scale).clamp(0.0, 1.0) };

    let c = if self.chat_muted || self.master_muted { 0.0 } else { (self.chat_volume * chat_scale).clamp(0.0, 1.0) };

    (g, c)
  }

  pub fn get_media_volume(&self) -> f32 {
    if self.media_muted || self.master_muted {
      0.0
    } else {
      self.media_volume.clamp(0.0, 1.0)
    }
  }

  /// Apply calculated volumes to live Windows Audio System and Sessions
  pub fn apply_mix(&mut self) {
    // 1. MASTER: Sync master volume to Windows Hardware Endpoint
    if (self.master_volume - self.last_applied_master_vol).abs() > 0.005 {
      if let Some(ref id) = self.master_device_id {
        set_endpoint_volume(id, self.master_volume);
      } else {
        set_system_master_volume(self.master_volume);
      }
      self.last_applied_master_vol = self.master_volume;
    }
    if self.master_muted != self.last_applied_master_muted {
      if let Some(ref id) = self.master_device_id {
        set_endpoint_mute(id, self.master_muted);
      } else {
        set_system_master_mute(self.master_muted);
      }
      self.last_applied_master_muted = self.master_muted;
    }

    // 2. GAME: Sync to dedicated Game endpoint if present
    if let Some(ref id) = self.game_device_id {
      if (self.game_volume - self.last_applied_game_vol).abs() > 0.005 {
        set_endpoint_volume(id, self.game_volume);
        self.last_applied_game_vol = self.game_volume;
      }
      if self.game_muted != self.last_applied_game_muted {
        set_endpoint_mute(id, self.game_muted);
        self.last_applied_game_muted = self.game_muted;
      }
    }

    // 3. CHAT: Sync to dedicated Chat endpoint if present
    if let Some(ref id) = self.chat_device_id {
      if (self.chat_volume - self.last_applied_chat_vol).abs() > 0.005 {
        set_endpoint_volume(id, self.chat_volume);
        self.last_applied_chat_vol = self.chat_volume;
      }
      if self.chat_muted != self.last_applied_chat_muted {
        set_endpoint_mute(id, self.chat_muted);
        self.last_applied_chat_muted = self.chat_muted;
      }
    }

    // 4. MEDIA: Sync to dedicated Media endpoint if present
    if let Some(ref id) = self.media_device_id {
      if (self.media_volume - self.last_applied_media_vol).abs() > 0.005 {
        set_endpoint_volume(id, self.media_volume);
        self.last_applied_media_vol = self.media_volume;
      }
      if self.media_muted != self.last_applied_media_muted {
        set_endpoint_mute(id, self.media_muted);
        self.last_applied_media_muted = self.media_muted;
      }
    }

    // 5. MIC: Sync microphone volume to physical microphone and default capture endpoint
    let target_hw_mic = self.mic_volume.min(1.0);
    if (target_hw_mic - self.last_applied_mic_vol).abs() > 0.005 {
      if let Some(ref id) = self.mic_device_id {
        set_endpoint_volume(id, target_hw_mic);
      }
      set_system_mic_volume(target_hw_mic);
      self.last_applied_mic_vol = target_hw_mic;
    }
    if self.mic_muted != self.last_applied_mic_muted {
      if let Some(ref id) = self.mic_device_id {
        set_endpoint_mute(id, self.mic_muted);
      }
      set_system_mic_mute(self.mic_muted);
      self.last_applied_mic_muted = self.mic_muted;
    }

    // 6. Only sync individual application process volumes via WASAPI sessions
    // if there are NO dedicated channel endpoints (fallback mode) and ChatMix balance is active!
    // Never overwrite application process volumes on launch or when dedicated endpoints are used.
    if self.game_device_id.is_none() && self.chat_device_id.is_none() && self.balance.abs() > 0.02 {
      self.apply_session_mix();
    }
  }

  /// Sync individual application process volumes via WASAPI sessions
  pub fn apply_session_mix(&self) {
    let (game_vol, chat_vol) = self.get_multipliers();
    let media_vol = self.get_media_volume();

    unsafe {
      let sessions = self.enumerate_active_sessions_raw();
      for (session_vol, bus) in sessions {
        let target_vol = match bus {
          AudioBus::Game => game_vol,
          AudioBus::Chat => chat_vol,
          AudioBus::Media => media_vol,
          AudioBus::Master => 1.0, // Master volume is handled at the hardware endpoint level
          AudioBus::Mic => 1.0,
        };
        let _ = session_vol.SetMasterVolume(target_vol, std::ptr::null());
      }
    }
  }

  pub fn list_active_sessions(&self) -> Vec<AudioSessionInfo> {
    let mut result = Vec::new();
    let mut seen_pids = HashSet::new();
    unsafe {
      for manager in get_all_session_managers() {
        if let Ok(enumerator) = manager.GetSessionEnumerator() {
          let count = enumerator.GetCount().unwrap_or(0);
          for i in 0..count {
            if let Ok(control) = enumerator.GetSession(i) {
              if let Ok(control2) = control.cast::<IAudioSessionControl2>() {
                if let Ok(pid) = control2.GetProcessId() {
                  if pid > 0 && !seen_pids.contains(&pid) {
                    seen_pids.insert(pid);
                    let exe_name = get_exe_name_from_pid(pid).unwrap_or_else(|| format!("PID: {}", pid));
                    if is_own_exe(&exe_name) {
                      continue;
                    }
                    let bus = self.classify_exe(&exe_name);
                    let mut vol = 1.0;
                    let mut muted = false;
                    if let Ok(vol_ctl) = control.cast::<ISimpleAudioVolume>() {
                      if let Ok(v) = vol_ctl.GetMasterVolume() {
                        vol = v;
                      }
                      if let Ok(m) = vol_ctl.GetMute() {
                        muted = m.as_bool();
                      }
                    }
                    result.push(AudioSessionInfo { pid, exe_name, volume: vol, is_muted: muted, bus });
                  }
                }
              }
            }
          }
        }
      }
    }
    result
  }

  unsafe fn enumerate_active_sessions_raw(&self) -> Vec<(ISimpleAudioVolume, AudioBus)> {
    let mut list = Vec::new();
    for manager in get_all_session_managers() {
      if let Ok(enumerator) = manager.GetSessionEnumerator() {
        let count = enumerator.GetCount().unwrap_or(0);
        for i in 0..count {
          if let Ok(control) = enumerator.GetSession(i) {
            if let Ok(control2) = control.cast::<IAudioSessionControl2>() {
              if let Ok(pid) = control2.GetProcessId() {
                if pid > 0 {
                  let exe_name = get_exe_name_from_pid(pid).unwrap_or_default();
                  if is_own_exe(&exe_name) {
                    continue;
                  }
                  let bus = self.classify_exe(&exe_name);
                  if let Ok(vol_ctl) = control.cast::<ISimpleAudioVolume>() {
                    list.push((vol_ctl, bus));
                  }
                }
              }
            }
          }
        }
      }
    }
    list
  }
}

pub fn get_endpoint_volume(device_id: &str) -> Option<(f32, bool)> {
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
    let device: IMMDevice = enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())).ok()?;
    let endpoint_vol: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
    let level = endpoint_vol.GetMasterVolumeLevelScalar().ok()?;
    let muted = endpoint_vol.GetMute().ok()?;
    Some((level, muted.as_bool()))
  }
}

pub fn set_endpoint_volume(device_id: &str, level: f32) {
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    if let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
      let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
      if let Ok(device) = enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())) {
        if let Ok(endpoint_vol) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
          let _ = endpoint_vol.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null());
        }
      }
    }
  }
}

pub fn set_endpoint_mute(device_id: &str, muted: bool) {
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    if let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
      let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
      if let Ok(device) = enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())) {
        if let Ok(endpoint_vol) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
          let _ = endpoint_vol.SetMute(BOOL::from(muted), std::ptr::null());
        }
      }
    }
  }
}

pub fn get_system_master_volume() -> Option<(f32, bool)> {
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
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    if let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
      if let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) {
        if let Ok(endpoint_vol) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
          let _ = endpoint_vol.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null());
        }
      }
    }
  }
}

pub fn set_system_master_mute(muted: bool) {
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    if let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
      if let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) {
        if let Ok(endpoint_vol) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
          let _ = endpoint_vol.SetMute(BOOL::from(muted), std::ptr::null());
        }
      }
    }
  }
}

pub fn get_system_mic_volume() -> Option<(f32, bool)> {
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
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
    let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole).ok()?;
    let meter: IAudioMeterInformation = device.Activate(CLSCTX_ALL, None).ok()?;
    meter.GetPeakValue().ok()
  }
}

pub fn get_endpoint_peak(device_id: &str) -> Option<f32> {
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
    let device: IMMDevice = enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())).ok()?;
    let meter: IAudioMeterInformation = device.Activate(CLSCTX_ALL, None).ok()?;
    meter.GetPeakValue().ok()
  }
}

pub fn set_system_mic_volume(level: f32) {
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    if let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
      if let Ok(device) = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) {
        if let Ok(endpoint_vol) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
          let _ = endpoint_vol.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null());
        }
      }
    }
  }
}

pub fn set_system_mic_mute(muted: bool) {
  unsafe {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    if let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
      if let Ok(device) = enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) {
        if let Ok(endpoint_vol) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
          let _ = endpoint_vol.SetMute(BOOL::from(muted), std::ptr::null());
        }
      }
    }
  }
}

/// This app's own exe (the GUI and the background daemon run from the same file). Its audio
/// is not an app the user mixes: it stays out of the channel lists and keeps its own volume.
fn is_own_exe(exe: &str) -> bool {
  static OWN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
  OWN
    .get_or_init(|| std::env::current_exe().ok()?.file_name().map(|n| n.to_string_lossy().into_owned()))
    .as_deref()
    .is_some_and(|own| own.eq_ignore_ascii_case(exe))
}

unsafe fn get_all_session_managers() -> Vec<IAudioSessionManager2> {
  let mut managers = Vec::new();
  let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
  if let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
    if let Ok(collection) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
      if let Ok(count) = collection.GetCount() {
        for i in 0..count {
          if let Ok(device) = collection.Item(i) {
            if let Ok(mgr) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) {
              managers.push(mgr);
            }
          }
        }
      }
    }
  }
  managers
}

pub(crate) fn get_exe_name_from_pid(pid: u32) -> Option<String> {
  unsafe {
    let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    if handle.is_invalid() {
      return None;
    }

    let mut buf = [0u16; 1024];
    let len = K32GetProcessImageFileNameW(handle, &mut buf);
    let _ = CloseHandle(handle);

    if len > 0 {
      let path = String::from_utf16_lossy(&buf[..len as usize]);
      let filename = path.split('\\').last().unwrap_or(&path).to_string();
      return Some(filename);
    }
    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

  #[test]
  fn test_system_volume_query() {
    unsafe {
      let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let master = get_system_master_volume();
    let mic = get_system_mic_volume();
    let mic_peak = get_system_mic_peak();
    println!("Current Master system volume: {:?}", master);
    println!("Current Mic system volume: {:?}", mic);
    println!("Current Mic peak level: {:?}", mic_peak);
    assert!(master.is_some(), "Should be able to query system master volume on Windows");
    assert!(mic_peak.is_some(), "Should be able to query mic peak on Windows");
  }

  #[test]
  #[ignore = "changes the real Windows mic volume; run manually: cargo test -- --ignored"]
  fn test_set_realtek_mic_volume() {
    let mic_id = "{0.0.1.00000000}.{cf0a2270-1885-4dd6-82a5-f636e9efadd5}";
    let initial = get_endpoint_volume(mic_id).unwrap();
    println!("Initial Realtek mic: {:?}", initial);

    // Set to 1.0 (100% / +22.5 dB)
    set_endpoint_volume(mic_id, 1.0);
    let after_set = get_endpoint_volume(mic_id).unwrap();
    println!("After setting to 1.0: {:?}", after_set);
    assert!((after_set.0 - 1.0).abs() < 0.05);

    // Restore to initial
    set_endpoint_volume(mic_id, initial.0);
    let restored = get_endpoint_volume(mic_id).unwrap();
    println!("Restored to: {:?}", restored);
  }
}
