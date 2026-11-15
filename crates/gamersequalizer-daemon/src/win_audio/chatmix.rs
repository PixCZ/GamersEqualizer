use super::app_channels::{self, AppChannel};
use super::endpoint_lock::{get_endpoint_info, AudioEndpointInfo};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::thread::LocalKey;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
  eCapture, eConsole, eRender, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
  IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::ProcessStatus::K32GetProcessImageFileNameW;
use windows::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_core::Interface;

/// `QueryHardwareSupport` bit: the device applies the volume itself (endpointvolume.h; the
/// windows crate doesn't export it).
const ENDPOINT_HARDWARE_SUPPORT_VOLUME: u32 = 0x1;
/// Session volumes are re-checked this often for new sessions (an app that starts playing);
/// a channel change applies at once.
const SESSION_MIX_INTERVAL: Duration = Duration::from_secs(1);
/// Volume steps smaller than this are "the same" (rounding of the permille values).
const VOLUME_EPSILON: f32 = 0.005;

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

/// Browsers stay on Media (`app_channels::channel_for`), even if game detection or a name match
/// says otherwise: YouTube plays in full quality. Lowercase exe file names.
pub const BROWSER_EXES: &[&str] = &[
  "chrome.exe",
  "chromium.exe",
  "msedge.exe",
  "firefox.exe",
  "librewolf.exe",
  "waterfox.exe",
  "floorp.exe",
  "zen.exe",
  "brave.exe",
  "opera.exe",
  "vivaldi.exe",
  "arc.exe",
  "thorium.exe",
  "iexplore.exe",
  // Privacy / Firefox forks (Tor Browser runs as firefox.exe)
  "mullvadbrowser.exe",
  "palemoon.exe",
  "basilisk.exe",
  "seamonkey.exe",
  "falkon.exe",
  "midori.exe",
  "k-meleon.exe",
  "icedragon.exe",
  "dooble.exe",
  "otter-browser.exe",
  // Chromium forks and bundled browsers (Opera GX / Air run as opera.exe)
  "whale.exe",
  "dragon.exe",
  "iron.exe",
  "slimjet.exe",
  "maxthon.exe",
  "avastbrowser.exe",
  "avgbrowser.exe",
  "ccleanerbrowser.exe",
  "nortonbrowser.exe",
  "sidekick.exe",
  "wavebox.exe",
  "360se.exe",
  "360chrome.exe",
  "qqbrowser.exe",
  "sogouexplorer.exe",
  "ucbrowser.exe",
  "aloha.exe",
  // Yandex Browser and Coc Coc: a generic name, but a wrong hit only keeps an app on Media (the default)
  "browser.exe",
  // AI browsers
  "comet.exe",
];

/// Apps on the Chat channel (ChatMix volume, chat stream EQ). Lowercase exe file names; active
/// desktop clients only. Anything missing stays on Media until the user drags it in the mixer
/// (`app_channels`).
pub const CHAT_EXES: &[&str] = &[
  // Voice chat for gamers
  "discord.exe",
  "discordptb.exe",
  "discordcanary.exe",
  "vesktop.exe",
  "ts3client_win64.exe",
  "ts3client_win32.exe",
  "teamspeak.exe",
  "mumble.exe",
  "guilded.exe",
  "ventrilo.exe",
  "teamtalk5.exe",
  "revolt.exe",
  // Messengers
  "signal.exe",
  "telegram.exe",
  "whatsapp.exe",
  "whatsapp.root.exe",
  "viber.exe",
  "element.exe",
  "wire.exe",
  "line.exe",
  "wechat.exe",
  "weixin.exe",
  "threema.exe",
  "qq.exe",
  "kakaotalk.exe",
  "pidgin.exe",
  "trillian.exe",
  "paltalk.exe",
  "jami.exe",
  "linphone.exe",
  "whatsappdesktop.exe",
  "zalo.exe",
  "camfrog video chat.exe",
  // Work / meetings
  "teams.exe",
  "ms-teams.exe",
  "slack.exe",
  "zoom.exe",
  "webex.exe",
  "ciscocollabhost.exe",
  "atmgr.exe",
  "mattermost.exe",
  "rocket.chat.exe",
  "ringcentral.exe",
  "zulip.exe",
  "lync.exe",
  "ciscojabber.exe",
  "webexhost.exe",
  "g2mvideoconference.exe",
  "ringcentralvideo.exe",
  "jitsi meet.exe",
  "g2mcomm.exe",
  "goto.exe",
  "bluejeans.exe",
  "dingtalk.exe",
  "vsee.exe",
  "alfaview.exe",
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
  /// Windows mic volume the automatic input gain set (`mic_volume` stays what the user or an app
  /// asked for). Seeing exactly this level in Windows is ours, anything else is someone's wish.
  pub mic_hw_auto: Option<f32>,
  /// When someone else (user, app) last changed the mic volume: the automatic gain waits.
  pub mic_changed_at: Option<Instant>,

  // Track last applied values to avoid redundant COM calls
  last_applied_master_vol: f32,
  last_applied_master_muted: bool,
  last_applied_game_vol: f32,
  last_applied_game_muted: bool,
  last_applied_chat_vol: f32,
  last_applied_chat_muted: bool,
  last_applied_media_vol: f32,
  last_applied_media_muted: bool,
  /// Mic volume this process last wrote (or saw) in Windows; `restore_mixer` sets it so a
  /// window started next to the service does not write its own reading over the wish.
  pub(crate) last_applied_mic_vol: f32,
  last_applied_mic_muted: bool,

  /// Off in the window while the background service runs: one process owns session volumes.
  pub sessions_enabled: bool,
  /// Per session (instance id): the volume this process last set. A session is only touched
  /// again when its channel's target moves away from that, so a level the user picks in the
  /// Windows mixer stays until they move our channel slider.
  session_set: HashMap<String, f32>,
  last_session_targets: Option<(f32, f32, f32)>,
  last_session_pass: Option<Instant>,
}

impl ChatMixController {
  #[expect(clippy::new_without_default, reason = "new() reads the system volumes, not a plain default")]
  pub fn new() -> Self {
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
      mic_hw_auto: None,
      mic_changed_at: None,
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
      sessions_enabled: true,
      session_set: HashMap::new(),
      last_session_targets: None,
      last_session_pass: None,
    }
  }

  /// The mic to work with: the Windows default, unless it is a virtual device (then the first
  /// physical one). Not the first in the list: on a laptop that is often a different mic.
  pub fn find_physical_microphone() -> Option<String> {
    unsafe {
      let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
      let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
      let default_id = enumerator
        .GetDefaultAudioEndpoint(eCapture, eConsole)
        .and_then(|d| d.GetId())
        .ok()
        .and_then(|id| id.to_string().ok());
      let collection = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE).ok()?;
      let count = collection.GetCount().ok()?;
      let devices: Vec<AudioEndpointInfo> = (0..count)
        .filter_map(|i| collection.Item(i).ok())
        .filter_map(|dev| get_endpoint_info(&dev, default_id.as_deref().unwrap_or_default()).ok())
        .collect();
      physical_endpoint_id(&devices, default_id.as_deref())
    }
  }

  /// Detect and bind specific endpoints for Master, Game, Chat, Media
  pub fn bind_endpoints(&mut self, devices: &[AudioEndpointInfo], default_id: Option<&str>) {
    // 1. Identify virtual channel endpoints (levels read when they changed, else here)
    if !self.bind_channel_endpoints(devices) {
      self.read_channel_levels();
    }

    // 2. Identify physical Master endpoint
    self.master_device_id = physical_endpoint_id(devices, default_id);
    self.read_bound_levels();
  }

  /// Only the Game / Chat / Media outputs (Sonar, VB-Cable), also for devices that appear or go
  /// after start: the mic choice and Master stay. True if one changed; the bound channels then
  /// take their endpoint's level. With one bound, `apply_session_mix` stops setting per-app
  /// volumes.
  pub fn bind_channel_endpoints(&mut self, devices: &[AudioEndpointInfo]) -> bool {
    let before = (self.game_device_id.clone(), self.chat_device_id.clone(), self.media_device_id.clone());
    self.game_device_id = devices
      .iter()
      .find(|d| {
        let n = d.name.to_lowercase();
        is_virtual_endpoint(&n) && (n.contains("game") || n.contains("gaming"))
      })
      .map(|d| d.id.clone());

    self.chat_device_id = devices
      .iter()
      .find(|d| {
        let n = d.name.to_lowercase();
        is_virtual_endpoint(&n) && (n.contains("chat") || n.contains("cable input"))
      })
      .map(|d| d.id.clone());

    self.media_device_id = devices
      .iter()
      .find(|d| {
        let n = d.name.to_lowercase();
        is_virtual_endpoint(&n) && n.contains("media")
      })
      .map(|d| d.id.clone());
    let changed = before != (self.game_device_id.clone(), self.chat_device_id.clone(), self.media_device_id.clone());
    if changed {
      self.read_channel_levels();
    }
    changed
  }

  /// Master level (the mic is resolved only by `use_mic`).
  fn read_bound_levels(&mut self) {
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
  }

  /// The only place the mic in use is resolved: the user's pick, else the Windows default
  /// (`find_physical_microphone`). Its volume is read only when the device changes, so a
  /// re-bind never copies another device's volume onto it (a webcam as the Windows default and
  /// a picked headset: the headset went 53 % -> 40 %). No mic found keeps the current one.
  /// True if the mic changed. An unchanged pick touches no COM.
  pub fn use_mic(&mut self, pick: Option<String>) -> bool {
    if pick.is_some() && pick == self.mic_device_id {
      return false;
    }
    let id = pick.or_else(Self::find_physical_microphone);
    if id.is_none() || id == self.mic_device_id {
      return false;
    }
    if let Some((v, m)) = id.as_deref().and_then(get_endpoint_volume) {
      (self.mic_volume, self.mic_muted) = (v, m);
      (self.last_applied_mic_vol, self.last_applied_mic_muted) = (v, m);
    }
    self.mic_device_id = id;
    true
  }

  /// Game / Chat / Media levels from their own endpoints, where bound.
  fn read_channel_levels(&mut self) {
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

    // 5. MIC: Sync with the mic in use only; without one there is nothing to follow (the
    // default capture device may be another mic, whose volume is not this one's wish).
    let mic_vol_opt = self.mic_device_id.as_deref().and_then(get_endpoint_volume);
    if let Some((sys_mic_vol, sys_mic_muted)) = mic_vol_opt {
      let expected = self.mic_hw_auto.unwrap_or(self.mic_volume.min(1.0));
      if (sys_mic_vol - expected).abs() > 0.015 {
        // Someone else moved it: that is the volume they want, the automatic gain steps back.
        self.mic_volume = sys_mic_vol;
        self.last_applied_mic_vol = sys_mic_vol;
        self.mic_hw_auto = None;
        self.mic_changed_at = Some(Instant::now());
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

  /// The automatic input gain sets the Windows mic volume to `level` (the wish stays).
  pub fn set_mic_hw_auto(&mut self, device_id: &str, level: f32) {
    set_endpoint_volume(device_id, level);
    self.mic_hw_auto = Some(level);
  }

  /// Automatic input gain off: Windows gets the volume the user/app asked for back.
  pub fn clear_mic_hw_auto(&mut self, device_id: &str) {
    if self.mic_hw_auto.take().is_some() {
      let level = self.mic_volume.min(1.0);
      set_endpoint_volume(device_id, level);
      self.last_applied_mic_vol = level;
    }
  }

  /// Take over a wish + automatic level from another process (window start, service restart):
  /// only while Windows still shows that automatic level, else someone changed it meanwhile.
  pub fn adopt_mic_auto(&mut self, wish: f32, auto: f32) {
    if (self.mic_volume - auto).abs() <= 0.015 {
      self.mic_volume = wish;
      self.last_applied_mic_vol = wish.min(1.0);
      self.mic_hw_auto = Some(auto);
    }
  }

  pub fn set_balance(&mut self, balance: f32) {
    self.balance = balance.clamp(-1.0, 1.0);
    self.apply_mix();
  }

  /// See `app_channels::channel_for`.
  pub fn classify_exe(&self, exe: &str) -> AudioBus {
    match app_channels::channel_for(exe) {
      AppChannel::Chat => AudioBus::Chat,
      AppChannel::Game => AudioBus::Game,
      AppChannel::Media | AppChannel::Unknown => AudioBus::Media,
    }
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

    // 5. MIC: Sync microphone volume to the mic in use, never to the default capture device
    // (that copied one mic's volume onto another: laptop mic + USB headset, webcam).
    if let Some(ref id) = self.mic_device_id {
      let target_hw_mic = self.mic_volume.min(1.0);
      if (target_hw_mic - self.last_applied_mic_vol).abs() > 0.005 {
        set_endpoint_volume(id, target_hw_mic);
        self.last_applied_mic_vol = target_hw_mic;
      }
      if self.mic_muted != self.last_applied_mic_muted {
        set_endpoint_mute(id, self.mic_muted);
        self.last_applied_mic_muted = self.mic_muted;
      }
    }

    // 6. Per-app session volumes (fallback mode without dedicated channel endpoints).
    self.apply_session_mix();
  }

  /// Channel volumes as per-app session volumes, only in fallback mode: with dedicated channel
  /// endpoints (Sonar-like) the endpoint carries the level and a session volume would multiply
  /// it (Game 50 % playing at 25 %). Throttled: a full pass when a target changed, else at most
  /// once per `SESSION_MIX_INTERVAL` (picks up new sessions).
  ///
  /// Never fights the user: a session is set only when it has no level from us yet and its
  /// target is not neutral (100 %), or when its target moved since we last set it.
  pub fn apply_session_mix(&mut self) {
    if !self.sessions_enabled || self.game_device_id.is_some() || self.chat_device_id.is_some() {
      return;
    }
    let (game_vol, chat_vol) = self.get_multipliers();
    let targets = (game_vol, chat_vol, self.get_media_volume());
    let changed = self.last_session_targets != Some(targets);
    if !changed && self.last_session_pass.is_some_and(|t| t.elapsed() < SESSION_MIX_INTERVAL) {
      return;
    }
    self.last_session_targets = Some(targets);
    self.last_session_pass = Some(Instant::now());

    let mut seen = HashMap::with_capacity(self.session_set.len());
    // SAFETY: COM calls on interfaces enumerated just now.
    unsafe {
      for (key, session_vol, bus) in self.enumerate_active_sessions_raw() {
        let target = match bus {
          AudioBus::Game => targets.0,
          AudioBus::Chat => targets.1,
          AudioBus::Media => targets.2,
          // Master is the hardware endpoint; never touched per app.
          AudioBus::Master | AudioBus::Mic => continue,
        };
        match session_action(self.session_set.get(&key).copied(), target) {
          SessionAction::Keep(last) => {
            if let Some(last) = last {
              seen.insert(key, last);
            }
          }
          SessionAction::Set => {
            let current = session_vol.GetMasterVolume().unwrap_or(-1.0);
            if (current - target).abs() > VOLUME_EPSILON {
              if let Err(e) = session_vol.SetMasterVolume(target, std::ptr::null()) {
                log::debug!("[ChatMix] SetMasterVolume failed: {e}");
                continue;
              }
            }
            seen.insert(key, target);
          }
        }
      }
    }
    // Sessions that ended are forgotten; a reopened app is a new session.
    self.session_set = seen;
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

  /// (session instance id, volume control, channel) of every app session on every device.
  unsafe fn enumerate_active_sessions_raw(&self) -> Vec<(String, ISimpleAudioVolume, AudioBus)> {
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
                  let key = session_instance_id(&control2).unwrap_or_else(|| format!("pid:{pid}:{i}"));
                  if let Ok(vol_ctl) = control.cast::<ISimpleAudioVolume>() {
                    list.push((key, vol_ctl, bus));
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

#[derive(Debug, PartialEq)]
enum SessionAction {
  /// Leave the session alone; remember the level we set before (if any).
  Keep(Option<f32>),
  Set,
}

/// `last`: the level this process set on the session before (None: never touched it).
fn session_action(last: Option<f32>, target: f32) -> SessionAction {
  match last {
    Some(l) if (l - target).abs() <= VOLUME_EPSILON => SessionAction::Keep(Some(l)),
    Some(_) => SessionAction::Set,
    // Neutral channel: the app keeps the level Windows / the user gave it.
    None if (target - 1.0).abs() <= VOLUME_EPSILON => SessionAction::Keep(None),
    None => SessionAction::Set,
  }
}

unsafe fn session_instance_id(control: &IAudioSessionControl2) -> Option<String> {
  let p = control.GetSessionInstanceIdentifier().ok()?;
  let id = p.to_string().ok();
  CoTaskMemFree(Some(p.0 as *const _));
  id
}

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
  let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
  let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
  let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
  let device: IMMDevice = enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())).ok()?;
  device.Activate::<I>(CLSCTX_ALL, None).ok()
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
      // Our own streams (mic bridge, speaker loopback) don't count.
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
          let _ = endpoint_vol.SetMute(muted, std::ptr::null());
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
  // SAFETY: COM call on a live interface.
  with_endpoint(&ENDPOINT_METERS, device_id, |m: &IAudioMeterInformation| unsafe { m.GetPeakValue() })
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
      let path = String::from_utf16_lossy(buf.get(..len as usize).unwrap_or_default());
      let filename = path.rsplit('\\').next().unwrap_or(&path).to_string();
      return Some(filename);
    }
    None
  }
}

/// Endpoint names of virtual devices (Sonar channels, VB-Cable, VR runtimes, NVIDIA Broadcast,
/// Krisp; "steam" also covers Steam Streaming). Heuristic by name: a physical device with
/// "virtual"/"cable" in its name would be skipped. A gaming headset is physical, so "game" is not
/// on the list (channel binding adds it on top).
pub fn is_virtual_endpoint(name_lowercase: &str) -> bool {
  ["sonar", "virtual", "cable", "vb-audio", "voicemeeter", "steam", "oculus", "nvidia broadcast", "krisp"]
    .iter()
    .any(|v| name_lowercase.contains(v))
}

/// The device to work with: the Windows default; only when that is virtual, the first physical one.
pub fn physical_endpoint_id(devices: &[AudioEndpointInfo], default_id: Option<&str>) -> Option<String> {
  let default_is_virtual = default_id
    .and_then(|def_id| devices.iter().find(|d| d.id == def_id))
    .is_some_and(|d| is_virtual_endpoint(&d.name.to_lowercase()));
  if !default_is_virtual {
    return default_id.map(|s| s.to_string());
  }
  devices
    .iter()
    .find(|d| !is_virtual_endpoint(&d.name.to_lowercase()))
    .map(|d| d.id.clone())
    .or_else(|| default_id.map(|s| s.to_string()))
}

#[cfg(test)]
mod tests {
  use super::*;
  use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

  #[test]
  fn session_volume_is_left_to_the_user_unless_our_target_moves() {
    // Never touched, neutral channel: the app keeps what the user set in the Windows mixer.
    assert_eq!(session_action(None, 1.0), SessionAction::Keep(None));
    // Never touched, channel lowered: set it.
    assert_eq!(session_action(None, 0.5), SessionAction::Set);
    // Set before, target unchanged: a later change by the user is not undone.
    assert_eq!(session_action(Some(0.5), 0.5), SessionAction::Keep(Some(0.5)));
    // Our target moved (also back to 100 %): apply it.
    assert_eq!(session_action(Some(0.5), 1.0), SessionAction::Set);
  }

  #[test]
  #[ignore = "needs a real output and mic (CI runners have none); run manually: cargo test -- --ignored"]
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

  #[test]
  fn app_lists_are_lowercase_exes_without_duplicates_or_overlap() {
    let mut seen = HashSet::new();
    for exe in CHAT_EXES.iter().chain(BROWSER_EXES) {
      assert!(exe.ends_with(".exe") && *exe == exe.to_ascii_lowercase(), "{exe}: lookups are lowercase");
      assert!(seen.insert(*exe), "{exe} is listed twice (or on both channels)");
    }
  }

  #[test]
  fn master_follows_a_physical_default_and_skips_a_virtual_one() {
    let dev = |id: &str, name: &str| AudioEndpointInfo { id: id.into(), name: name.into(), is_default: false };
    let devices =
      [dev("cable", "CABLE Input (VB-Audio Virtual Cable)"), dev("phones", "Headphones"), dev("monitor", "LG Monitor")];
    assert_eq!(physical_endpoint_id(&devices, Some("monitor")).as_deref(), Some("monitor"));
    assert_eq!(physical_endpoint_id(&devices, Some("phones")).as_deref(), Some("phones"));
    assert_eq!(
      physical_endpoint_id(&devices, Some("cable")).as_deref(),
      Some("phones"),
      "virtual default: first physical"
    );
    assert_eq!(physical_endpoint_id(&devices, None), None);
  }

  /// Tester report: laptop mic first in the list, USB headset as the default. The headset wins.
  #[test]
  fn endpoint_is_the_default_not_the_first_and_a_gaming_headset_is_physical() {
    let dev = |id: &str, name: &str| AudioEndpointInfo { id: id.into(), name: name.into(), is_default: false };
    let mics =
      [dev("array", "Microphone Array (Realtek(R) Audio)"), dev("usb", "Microphone (HyperX Cloud Gaming Headset)")];
    assert_eq!(physical_endpoint_id(&mics, Some("usb")).as_deref(), Some("usb"));
    let outs = [dev("speakers", "Speakers (Realtek(R) Audio)"), dev("usb", "Headphones (Logitech G Gaming Headset)")];
    assert_eq!(physical_endpoint_id(&outs, Some("usb")).as_deref(), Some("usb"), "gaming headset is not virtual");
    assert!(is_virtual_endpoint("steelseries sonar - gaming"));
    assert!(!is_virtual_endpoint("headphones (logitech g gaming headset)"));
  }

  /// Regression: channel outputs were bound only at service start, so Sonar started later got
  /// per-app volumes on top of its own channel levels (Game 50 % playing at 25 %).
  #[test]
  fn channel_output_bound_late_stops_per_app_volumes() {
    let dev = |id: &str, name: &str| AudioEndpointInfo { id: id.into(), name: name.into(), is_default: false };
    let mut mix = ChatMixController::new();
    mix.mic_device_id = Some("mic".into());
    let (master, mic_volume) = (mix.master_device_id.clone(), mix.mic_volume);
    assert!(!mix.bind_channel_endpoints(&[dev("phones", "Headphones")]));
    let sonar = [dev("phones", "Headphones"), dev("game", "SteelSeries Sonar - Gaming")];
    assert!(mix.bind_channel_endpoints(&sonar));
    assert_eq!(mix.game_device_id.as_deref(), Some("game"));
    assert!(!mix.bind_channel_endpoints(&sonar), "unchanged devices: nothing to report");
    assert_eq!(
      (mix.master_device_id.clone(), mix.mic_device_id.as_deref(), mix.mic_volume),
      (master, Some("mic"), mic_volume)
    );
    // The guard returns before any session pass (which would stamp `last_session_pass`).
    mix.apply_session_mix();
    assert!(mix.last_session_pass.is_none());
  }
}
