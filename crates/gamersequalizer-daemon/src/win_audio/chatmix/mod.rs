use super::app_channels::{self, AppChannel};
use super::endpoint_lock::{AudioEndpointInfo, get_endpoint_info};
use super::mmdevice;
use gamersequalizer_common::shm::ShmHeader;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::LocalKey;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::{
  AudioSessionStateActive, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, ISimpleAudioVolume,
  eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::ProcessStatus::K32GetProcessImageFileNameW;
use windows::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_core::Interface;

mod endpoint_volume;

pub use endpoint_volume::*;

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

/// One mixer channel: what the user (or an app) wants, and what this process last wrote to or
/// saw in Windows, so an unchanged level touches no COM.
#[derive(Debug, Clone, Copy)]
pub struct Channel {
  pub volume: f32,
  pub muted: bool,
  /// `restore_mixer` sets it for the mic so a window started next to the service does not write
  /// its own reading over the wish.
  pub(crate) applied_volume: f32,
  applied_muted: bool,
}

pub struct ChatMixController {
  pub balance: f32, // -1.0 (100% Game, 0% Chat) .. 0.0 (Equal) .. +1.0 (0% Game, 100% Chat)
  pub master: Channel,
  pub game: Channel,
  pub chat: Channel,
  pub media: Channel,
  /// `volume` is the wish and may be a boost above 100 %; Windows gets at most 100 %.
  pub mic: Channel,

  // Dedicated hardware or virtual endpoints for channels
  pub master_device_id: Option<String>,
  pub game_device_id: Option<String>,
  pub chat_device_id: Option<String>,
  pub media_device_id: Option<String>,
  pub mic_device_id: Option<String>,
  pub mic_peak_level: f32,
  /// Windows mic volume the automatic input gain set (`mic.volume` stays what the user or an app
  /// asked for). Seeing exactly this level in Windows is ours, anything else is someone's wish.
  pub mic_hw_auto: Option<f32>,
  /// When someone else (user, app) last changed the mic volume: the automatic gain waits.
  pub mic_changed_at: Option<Instant>,

  /// Off in the window while the background service runs: one process owns session volumes.
  pub sessions_enabled: bool,
  /// Per session (instance id): the volume this process last set. A session is only touched
  /// again when its channel's target moves away from that, so a level the user picks in the
  /// Windows mixer stays until they move our channel slider.
  session_set: HashMap<String, f32>,
  last_session_targets: Option<(f32, f32, f32)>,
  last_session_pass: Option<Instant>,
}

impl Channel {
  fn new(volume: f32, muted: bool) -> Self {
    Self { volume, muted, applied_volume: volume, applied_muted: muted }
  }

  /// A level read from Windows becomes the wish (a newly bound endpoint).
  fn adopt(&mut self, (volume, muted): (f32, bool)) {
    *self = Self::new(volume, muted);
  }

  /// Someone else changed the level in Windows: that is the new wish. True if anything changed.
  fn follow(&mut self, (volume, muted): (f32, bool)) -> bool {
    let moved = (volume - self.applied_volume).abs() > VOLUME_EPSILON;
    if moved {
      (self.volume, self.applied_volume) = (volume, volume);
    }
    self.follow_mute(muted) || moved
  }

  fn follow_mute(&mut self, muted: bool) -> bool {
    let flipped = muted != self.applied_muted;
    if flipped {
      (self.muted, self.applied_muted) = (muted, muted);
    }
    flipped
  }

  /// Writes to Windows only what moved since the last write; Windows takes at most 100 %.
  fn push(&mut self, set_volume: impl FnOnce(f32), set_mute: impl FnOnce(bool)) {
    let target = self.volume.min(1.0);
    if (target - self.applied_volume).abs() > VOLUME_EPSILON {
      set_volume(target);
      self.applied_volume = target;
    }
    if self.muted != self.applied_muted {
      set_mute(self.muted);
      self.applied_muted = self.muted;
    }
  }

  pub fn write_to(&self, volume_permille: &AtomicU32, muted: &AtomicBool) {
    volume_permille.store((self.volume * 1000.0).round() as u32, Ordering::Relaxed);
    muted.store(self.muted, Ordering::Relaxed);
  }
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
      master: Channel::new(initial_master_vol, initial_master_muted),
      game: Channel::new(1.0, false),
      chat: Channel::new(1.0, false),
      media: Channel::new(1.0, false),
      mic: Channel::new(initial_mic_vol, initial_mic_muted),
      master_device_id: None,
      game_device_id: None,
      chat_device_id: None,
      media_device_id: None,
      mic_device_id: None,
      mic_peak_level: 0.0,
      mic_hw_auto: None,
      mic_changed_at: None,
      sessions_enabled: true,
      session_set: HashMap::new(),
      last_session_targets: None,
      last_session_pass: None,
    }
  }

  /// All five channels into the shared memory (the balance is the window's alone).
  pub fn write_levels(&self, h: &ShmHeader) {
    self.master.write_to(&h.master_volume_permille, &h.master_muted);
    self.game.write_to(&h.game_volume_permille, &h.game_muted);
    self.chat.write_to(&h.chat_volume_permille, &h.chat_muted);
    self.media.write_to(&h.media_volume_permille, &h.media_muted);
    self.mic.write_to(&h.mic_volume_permille, &h.mic_muted);
  }

  /// The mic to work with: the Windows default, unless it is a virtual device (then the first
  /// physical one). Not the first in the list: on a laptop that is often a different mic.
  pub fn find_physical_microphone() -> Option<String> {
    // SAFETY: COM is initialized on this thread first (an already initialized thread keeps its apartment); every
    // interface is created in this block and released when dropped.
    unsafe {
      let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
      let enumerator = mmdevice::enumerator().ok()?;
      let default_id =
        enumerator.GetDefaultAudioEndpoint(eCapture, eConsole).ok().and_then(|d| mmdevice::device_id(&d));
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
    let level = match self.master_device_id {
      Some(ref id) => get_endpoint_volume(id),
      None => get_system_master_volume(),
    };
    if let Some(level) = level {
      self.master.adopt(level);
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
    if let Some(level) = id.as_deref().and_then(get_endpoint_volume) {
      self.mic.adopt(level);
    }
    self.mic_device_id = id;
    true
  }

  /// Game / Chat / Media levels from their own endpoints, where bound.
  fn read_channel_levels(&mut self) {
    for (channel, id) in [
      (&mut self.game, &self.game_device_id),
      (&mut self.chat, &self.chat_device_id),
      (&mut self.media, &self.media_device_id),
    ] {
      if let Some(level) = id.as_deref().and_then(get_endpoint_volume) {
        channel.adopt(level);
      }
    }
  }

  /// Sync live volume/mute changes that occurred externally in Windows
  pub fn sync_from_system(&mut self) -> bool {
    let mut changed = false;

    let master = match self.master_device_id {
      Some(ref id) => get_endpoint_volume(id),
      None => get_system_master_volume(),
    };
    if let Some(level) = master {
      changed |= self.master.follow(level);
    }
    // Game / Chat / Media only where they have a dedicated endpoint.
    for (channel, id) in [
      (&mut self.game, &self.game_device_id),
      (&mut self.chat, &self.chat_device_id),
      (&mut self.media, &self.media_device_id),
    ] {
      if let Some(level) = id.as_deref().and_then(get_endpoint_volume) {
        changed |= channel.follow(level);
      }
    }

    // MIC: Sync with the mic in use only; without one there is nothing to follow (the
    // default capture device may be another mic, whose volume is not this one's wish).
    let mic_vol_opt = self.mic_device_id.as_deref().and_then(get_endpoint_volume);
    if let Some((sys_mic_vol, sys_mic_muted)) = mic_vol_opt {
      let expected = self.mic_hw_auto.unwrap_or(self.mic.volume.min(1.0));
      if (sys_mic_vol - expected).abs() > 0.015 {
        // Someone else moved it: that is the volume they want, the automatic gain steps back.
        self.mic.volume = sys_mic_vol;
        self.mic.applied_volume = sys_mic_vol;
        self.mic_hw_auto = None;
        self.mic_changed_at = Some(Instant::now());
        changed = true;
      }
      changed |= self.mic.follow_mute(sys_mic_muted);
    }

    // MIC PEAK: Read instantaneous audio peak level from physical mic
    let peak_opt = if let Some(ref id) = self.mic_device_id { get_endpoint_peak(id) } else { get_system_mic_peak() };
    if let Some(peak) = peak_opt {
      let boost = if self.mic.volume > 1.0 { self.mic.volume } else { 1.0 };
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
      let level = self.mic.volume.min(1.0);
      set_endpoint_volume(device_id, level);
      self.mic.applied_volume = level;
    }
  }

  /// Take over a wish + automatic level from another process (window start, service restart):
  /// only while Windows still shows that automatic level, else someone changed it meanwhile.
  pub fn adopt_mic_auto(&mut self, wish: f32, auto: f32) {
    if (self.mic.volume - auto).abs() <= 0.015 {
      self.mic.volume = wish;
      self.mic.applied_volume = wish.min(1.0);
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

    let g = if self.game.muted || self.master.muted { 0.0 } else { (self.game.volume * game_scale).clamp(0.0, 1.0) };

    let c = if self.chat.muted || self.master.muted { 0.0 } else { (self.chat.volume * chat_scale).clamp(0.0, 1.0) };

    (g, c)
  }

  /// Automatic ChatMix: how loud the listener hears Game and Chat (linear, 1000 = unchanged),
  /// relative to the app stream the stream APO sees, i.e. before the session volume.
  pub fn write_duck_gains(&self, h: &ShmHeader) {
    let (game, chat) = self.duck_gains();
    h.duck_game_gain_permille.store((game * 1000.0).round() as u32, Ordering::Relaxed);
    h.duck_chat_gain_permille.store((chat * 1000.0).round() as u32, Ordering::Relaxed);
  }

  pub fn get_media_volume(&self) -> f32 {
    if self.media.muted || self.master.muted { 0.0 } else { self.media.volume.clamp(0.0, 1.0) }
  }

  /// (game, chat) gain for `write_duck_gains`, from where each channel's volume is applied.
  /// Master scales both alike, so only its mute (in the session volumes) counts.
  fn duck_gains(&self) -> (f32, f32) {
    let sessions = self.game_device_id.is_none() && self.chat_device_id.is_none();
    let (game, chat) = self.get_multipliers();
    let gain = |channel: &Channel, device: &Option<String>, session: f32| match device {
      // shortcut: Windows' endpoint scalar is audio-tapered, not linear amplitude (50 % is well
      // under half); read `GetMasterVolumeLevel` (dB) if ducking on channel outputs (Sonar) ever
      // matters. There the virtual device mixes the apps, so the stream APO rarely sees them apart.
      Some(_) => {
        if channel.muted {
          0.0
        } else {
          channel.volume.clamp(0.0, 1.0)
        }
      }
      // Per-app session volumes carry the channel level and the ChatMix balance.
      None if sessions => session,
      // The other channel has its own output, so no session volume is set (`apply_session_mix`).
      None => 1.0,
    };
    (gain(&self.game, &self.game_device_id, game), gain(&self.chat, &self.chat_device_id, chat))
  }

  /// Apply calculated volumes to live Windows Audio System and Sessions
  pub fn apply_mix(&mut self) {
    match self.master_device_id {
      Some(ref id) => self.master.push(|v| set_endpoint_volume(id, v), |m| set_endpoint_mute(id, m)),
      None => self.master.push(set_system_master_volume, set_system_master_mute),
    }
    // Game / Chat / Media only where they have a dedicated endpoint (else `apply_session_mix`). The
    // mic only to the mic in use, never to the default capture device (that copied one mic's
    // volume onto another: laptop mic + USB headset, webcam).
    for (channel, id) in [
      (&mut self.game, &self.game_device_id),
      (&mut self.chat, &self.chat_device_id),
      (&mut self.media, &self.media_device_id),
      (&mut self.mic, &self.mic_device_id),
    ] {
      if let Some(id) = id {
        channel.push(|v| set_endpoint_volume(id, v), |m| set_endpoint_mute(id, m));
      }
    }

    // Per-app session volumes (fallback mode without dedicated channel endpoints).
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
            if (current - target).abs() > VOLUME_EPSILON
              && let Err(e) = session_vol.SetMasterVolume(target, std::ptr::null())
            {
              log::debug!("[ChatMix] SetMasterVolume failed: {e}");
              continue;
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
    // SAFETY: COM calls on the session interfaces from `get_all_session_managers` (which initializes COM on this
    // thread), used only inside this block.
    unsafe {
      for manager in get_all_session_managers() {
        if let Ok(enumerator) = manager.GetSessionEnumerator() {
          let count = enumerator.GetCount().unwrap_or(0);
          for i in 0..count {
            if let Ok(control) = enumerator.GetSession(i)
              && let Ok(control2) = control.cast::<IAudioSessionControl2>()
              && let Ok(pid) = control2.GetProcessId()
              && pid > 0
              && !seen_pids.contains(&pid)
            {
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
    result
  }

  /// (session instance id, volume control, channel) of every app session on every device.
  unsafe fn enumerate_active_sessions_raw(&self) -> Vec<(String, ISimpleAudioVolume, AudioBus)> {
    // SAFETY: the session interfaces come from `get_all_session_managers`, which initializes COM on this thread;
    // the returned ones carry their own references.
    unsafe {
      let mut list = Vec::new();
      for manager in get_all_session_managers() {
        if let Ok(enumerator) = manager.GetSessionEnumerator() {
          let count = enumerator.GetCount().unwrap_or(0);
          for i in 0..count {
            if let Ok(control) = enumerator.GetSession(i)
              && let Ok(control2) = control.cast::<IAudioSessionControl2>()
              && let Ok(pid) = control2.GetProcessId()
              && pid > 0
            {
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
      list
    }
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
  // SAFETY: a live session control; the identifier string it allocates is freed once below.
  unsafe {
    let p = control.GetSessionInstanceIdentifier().ok()?;
    let id = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as *const _));
    id
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
  // SAFETY: COM is initialized on this thread before the enumerator is created; the returned managers carry their
  // own references.
  unsafe {
    let mut managers = Vec::new();
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    if let Ok(enumerator) = mmdevice::enumerator()
      && let Ok(collection) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
      && let Ok(count) = collection.GetCount()
    {
      for i in 0..count {
        if let Ok(device) = collection.Item(i)
          && let Ok(mgr) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)
        {
          managers.push(mgr);
        }
      }
    }
    managers
  }
}

pub(crate) fn get_exe_name_from_pid(pid: u32) -> Option<String> {
  // SAFETY: the process handle is checked and closed below; the name buffer is a local array whose length the call
  // gets.
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

/// `physical_endpoint_id` after a device came or went while the Windows default stayed. Behind
/// a virtual default any physical device would do, so the one in use (`current`) stays while it
/// is connected: a monitor or USB device that only comes earlier in the list must not take the
/// Master channel and the echo reference from the headset.
pub fn master_endpoint_id(
  devices: &[AudioEndpointInfo],
  default_id: Option<&str>,
  current: Option<&str>,
) -> Option<String> {
  let picked = physical_endpoint_id(devices, default_id);
  let connected = |id: &str| devices.iter().any(|d| d.id == id && !is_virtual_endpoint(&d.name.to_lowercase()));
  match current {
    Some(id) if picked.as_deref() != default_id && connected(id) => Some(id.to_string()),
    _ => picked,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

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
    // SAFETY: COM init for the test thread, no reserved pointer.
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

  #[test]
  fn master_stays_on_its_device_when_another_one_is_plugged_in() {
    let dev = |id: &str, name: &str| AudioEndpointInfo { id: id.into(), name: name.into(), is_default: false };
    let devices =
      [dev("cable", "CABLE Input (VB-Audio Virtual Cable)"), dev("monitor", "LG Monitor"), dev("phones", "Headphones")];
    let master = |default, current| master_endpoint_id(&devices, Some(default), current);
    assert_eq!(master("cable", Some("phones")).as_deref(), Some("phones"), "not the monitor that comes first");
    assert_eq!(master("cable", Some("gone")).as_deref(), Some("monitor"), "unplugged: the first physical one");
    assert_eq!(master("cable", Some("cable")).as_deref(), Some("monitor"), "never a virtual one");
    assert_eq!(master("cable", None).as_deref(), Some("monitor"));
    assert_eq!(master("monitor", Some("phones")).as_deref(), Some("monitor"), "a physical default wins");
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
    let (master, mic_volume) = (mix.master_device_id.clone(), mix.mic.volume);
    assert!(!mix.bind_channel_endpoints(&[dev("phones", "Headphones")]));
    let sonar = [dev("phones", "Headphones"), dev("game", "SteelSeries Sonar - Gaming")];
    assert!(mix.bind_channel_endpoints(&sonar));
    assert_eq!(mix.game_device_id.as_deref(), Some("game"));
    assert!(!mix.bind_channel_endpoints(&sonar), "unchanged devices: nothing to report");
    assert_eq!(
      (mix.master_device_id.clone(), mix.mic_device_id.as_deref(), mix.mic.volume),
      (master, Some("mic"), mic_volume)
    );
    // The guard returns before any session pass (which would stamp `last_session_pass`).
    mix.apply_session_mix();
    assert!(mix.last_session_pass.is_none());
  }

  #[test]
  fn duck_gains_follow_where_each_channel_gets_its_volume() {
    let mut mix = ChatMixController::new();
    mix.master.muted = false;
    (mix.game.volume, mix.chat.volume, mix.balance) = (0.8, 1.0, 0.5);
    assert_eq!(mix.duck_gains(), (0.4, 1.0), "session volumes: channel level x balance");
    mix.chat.muted = true;
    assert_eq!(mix.duck_gains(), (0.4, 0.0));
    // Game on its own output (Sonar): that endpoint's level, no balance; chat gets no volume of ours.
    (mix.game_device_id, mix.chat.muted) = (Some("game".into()), false);
    assert_eq!(mix.duck_gains(), (0.8, 1.0));
  }

  #[test]
  fn channel_follows_windows_without_losing_an_unsent_mute() {
    let mut ch = Channel::new(0.5, false);
    ch.muted = true; // clicked, not pushed yet
    assert!(!ch.follow((0.502, false)), "rounding noise is no change");
    assert!(ch.muted && ch.volume == 0.5);
    assert!(ch.follow((0.3, true)));
    assert_eq!((ch.volume, ch.applied_volume, ch.muted, ch.applied_muted), (0.3, 0.3, true, true));
  }

  #[test]
  fn channel_push_writes_only_changes_and_caps_at_full() {
    let mut ch = Channel::new(1.0, false);
    let (mut volumes, mut mutes) = (Vec::new(), Vec::new());
    ch.volume = 2.0; // a mic boost
    ch.push(|v| volumes.push(v), |m| mutes.push(m));
    assert!(volumes.is_empty() && mutes.is_empty(), "Windows already sits at 100 %");
    ch.volume = 0.4;
    ch.muted = true;
    ch.push(|v| volumes.push(v), |m| mutes.push(m));
    ch.push(|v| volumes.push(v), |m| mutes.push(m));
    assert_eq!((volumes, mutes), (vec![0.4], vec![true]));
  }
}
