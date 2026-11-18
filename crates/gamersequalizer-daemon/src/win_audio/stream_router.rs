//! Tells each stream APO instance (one per app stream on the output, before the mix) which
//! channel it belongs to, so chat and media apps get their own EQ and Automatic ChatMix knows
//! which streams are the game and which the chat (`paired_streams`, read by `ducking`).
//!
//! The APO runs inside audiodg and can't see the app. This thread samples every audio
//! session's peak meter on the default output every 5 ms and pairs each instance with the
//! session whose meter its level envelope reproduces (`streams::hold_match`; measured:
//! the right app 93-100 %, others <= 6 %). The channel then comes from the app's exe, the
//! same lists the ChatMix volumes use.
//!
//! Game streams get `StreamBus::Game` (no stream EQ: the output APO carries the game EQ).
//! Unpaired streams (silent, just started, or audio that bypasses per-stream effects such as
//! Atmos spatial objects) stay on `StreamBus::None`: pass-through, i.e. headphone + game EQ
//! from the output APO, as before.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gamersequalizer_common::shm::APO_SHM_NAME;
use gamersequalizer_common::streams::{
  ENVELOPE_BLOCK_HNS, ENVELOPE_LEN, HOLD_PAD_AFTER, HOLD_PAD_BEFORE, SILENCE, StreamBus, hold_match, pick_session,
};
use gamersequalizer_mic_chain::shared_memory::SharedMemoryClient;
use log::{error, info};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
  AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::core::Interface;

use super::app_channels::{self, AppChannel};
use super::chatmix::{exe_name_of, get_exe_path_from_pid};
use super::console_owner::ConsoleOwner;

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

const SAMPLE_INTERVAL: Duration = Duration::from_millis(5);
/// Pairing / bus update cadence; also how often sessions and the default output are re-read.
const EVAL_INTERVAL: Duration = Duration::from_millis(500);
const OPEN_RETRY: Duration = Duration::from_secs(2);
const SLEEP_ON_ERROR: Duration = Duration::from_secs(1);
/// Meter history compared per evaluation (1 s), ending `HOLD_PAD_AFTER` blocks ago.
const MATCH_BLOCKS: usize = 100;
/// Per-session meter history kept (blocks); must cover the compared window.
const HISTORY: usize = 128;
const _: () = assert!(MATCH_BLOCKS + HOLD_PAD_AFTER < HISTORY);
const _: () = assert!(MATCH_BLOCKS + HOLD_PAD_BEFORE + HOLD_PAD_AFTER + 10 < ENVELOPE_LEN);

// ---------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------

/// One audio session on the default output, with its recent meter peaks.
struct Session {
  pid: u32,
  /// The app's image path, and its file name (what the channel lists match).
  path: String,
  exe: String,
  bus: StreamBus,
  meter: IAudioMeterInformation,
  history: PeakHistory,
}

/// A chat or game stream paired with its app.
#[derive(Debug, Clone, PartialEq)]
pub struct PairedStream {
  /// The slot's owner value (`StreamSlot::owner`).
  pub owner: u64,
  pub is_game: bool,
  pub exe: String,
  /// The app's image path (which game, for the mixer).
  pub path: String,
}

/// The chat and game streams as of the last evaluation. Automatic ChatMix runs on the reference
/// thread (`apo_link`) and needs the exe names, which only this thread knows: the table carries the
/// bus but no names. Held only to copy the list in or out.
static PAIRED: Mutex<Vec<PairedStream>> = Mutex::new(Vec::new());

/// Max meter peak per envelope block, the last `HISTORY` blocks.
struct PeakHistory {
  peaks: [f32; HISTORY],
  /// Block number each `peaks` entry belongs to.
  blocks: [u64; HISTORY],
}

#[derive(Default)]
struct Router {
  apo: Option<SharedMemoryClient>,
  last_open_try: Option<Instant>,
  manager: Option<(String, IAudioSessionManager2)>,
  /// Keyed by session instance identifier (stable across re-enumeration).
  sessions: HashMap<String, Session>,
  /// Slot owner value -> session key it was paired with.
  pairs: HashMap<u64, String>,
  /// Some stream EQ or Automatic ChatMix is on, so streams are metered and paired (logged on change).
  routing: bool,
  /// Only the console session's daemon pairs streams (the APO serves that user).
  console: ConsoleOwner,
}

// ---------------------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------------------

/// Spawn the supervised router thread (daemon lifetime).
pub fn spawn() -> Option<std::thread::JoinHandle<()>> {
  std::thread::Builder::new()
    .name("GamersEqualizer_StreamRouter".into())
    .spawn(|| {
      // SAFETY: COM init for this thread.
      unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
      }
      info!("[StreamRouter] started");
      let mut router = Router::default();
      loop {
        if let Err(e) = router.iteration() {
          error!("[StreamRouter] iteration failed: {e}");
          // Device gone / audio restarted: start over with fresh COM objects.
          router.manager = None;
          router.sessions.clear();
          std::thread::sleep(SLEEP_ON_ERROR);
        }
      }
    })
    .ok()
}

/// Stream bus of an app by its exe name. See `app_channels::channel_for`.
pub fn bus_for_exe(exe: &str) -> StreamBus {
  match app_channels::channel_for(exe) {
    AppChannel::Chat => StreamBus::Chat,
    AppChannel::Game => StreamBus::Game,
    AppChannel::Media | AppChannel::Unknown => StreamBus::Media,
  }
}

/// The paired chat and game streams (see `PAIRED`); empty if its lock was poisoned.
pub fn paired_streams() -> Vec<PairedStream> {
  PAIRED.lock().map(|p| p.clone()).unwrap_or_default()
}

/// Current QPC time in 100 ns units, the clock the APO stamps `last_audio_hns` with.
pub fn now_hns() -> Option<u64> {
  let (mut counter, mut freq) = (0i64, 0i64);
  // SAFETY: out parameters are locals.
  unsafe {
    QueryPerformanceCounter(&mut counter).ok()?;
    QueryPerformanceFrequency(&mut freq).ok()?;
  }
  u64::try_from(counter as i128 * 10_000_000 / i128::from(freq.max(1))).ok()
}

// ---------------------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------------------

impl Router {
  /// Sample meters for one evaluation period, then pair and publish buses.
  fn iteration(&mut self) -> Result<(), String> {
    self.ensure_apo();
    let Some(apo) = self.apo.as_ref() else {
      // No stream APO installed / no audio yet: nothing to route.
      std::thread::sleep(OPEN_RETRY);
      return Ok(());
    };
    if !self.console.is_owner() {
      std::thread::sleep(EVAL_INTERVAL);
      return Ok(());
    }
    // No chat / media EQ and no Automatic ChatMix: the APO passes every stream through, so skip
    // the 200 Hz metering. Known limit: after one is turned on, new pairings need ~1.5 s of fresh
    // meter history.
    let header = &apo.layout().header;
    let active = header.stream_eq_active() || header.duck_enabled.load(Ordering::Relaxed);
    if active != self.routing {
      self.routing = active;
      info!(
        "[StreamRouter] {}",
        if active {
          "chat/media EQ or Automatic ChatMix on: routing streams"
        } else {
          "no chat/media EQ, Automatic ChatMix off: idle"
        }
      );
    }
    if !active {
      std::thread::sleep(EVAL_INTERVAL);
      return Ok(());
    }
    self.refresh_sessions()?;
    let started = Instant::now();
    while started.elapsed() < EVAL_INTERVAL {
      let block = now_block().ok_or("QueryPerformanceCounter failed")?;
      for s in self.sessions.values_mut() {
        // SAFETY: COM call on an interface we own. A failed read counts as silence.
        let peak = unsafe { s.meter.GetPeakValue() }.unwrap_or(0.0);
        s.history.record(block, peak);
      }
      std::thread::sleep(SAMPLE_INTERVAL);
    }
    self.evaluate();
    Ok(())
  }

  fn ensure_apo(&mut self) {
    if self.apo.is_some() || self.last_open_try.is_some_and(|t| t.elapsed() < OPEN_RETRY) {
      return;
    }
    self.last_open_try = Some(Instant::now());
    // Fails until an APO has run once since boot / audio restart: expected, not logged.
    if let Ok(shm) = SharedMemoryClient::open_existing(APO_SHM_NAME) {
      info!("[StreamRouter] connected to the APO mapping");
      self.apo = Some(shm);
    }
  }

  /// Follow the default output and pick up new / gone sessions (meter history is kept).
  fn refresh_sessions(&mut self) -> Result<(), String> {
    // SAFETY: COM calls on interfaces we own; strings freed with CoTaskMemFree.
    unsafe {
      let device = super::mmdevice::default_device(eRender, eConsole).map_err(|e| format!("default output: {e}"))?;
      let id_text = super::mmdevice::device_id(&device).ok_or("default output: no readable id")?;
      if self.manager.as_ref().is_none_or(|(current, _)| *current != id_text) {
        let manager: IAudioSessionManager2 =
          device.Activate(CLSCTX_ALL, None).map_err(|e| format!("session manager: {e}"))?;
        info!("[StreamRouter] following output {id_text}");
        self.manager = Some((id_text, manager));
        self.sessions.clear();
        self.pairs.clear();
      }
      let Some((_, manager)) = self.manager.as_ref() else {
        return Ok(());
      };
      let list = manager.GetSessionEnumerator().map_err(|e| format!("session list: {e}"))?;
      let own_pid = GetCurrentProcessId();
      let mut seen = HashSet::new();
      for i in 0..list.GetCount().map_err(|e| format!("session count: {e}"))? {
        let Ok(control) = list.GetSession(i) else { continue };
        // Windows keeps a session for every app that ever played on this output; only the
        // playing ones can be paired, and metering the rest 200x a second was wasted RPCs.
        if control.GetState().ok() != Some(AudioSessionStateActive) {
          continue;
        }
        let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
        let Ok(raw_key) = control2.GetSessionInstanceIdentifier() else { continue };
        let key = raw_key.to_string().unwrap_or_default();
        CoTaskMemFree(Some(raw_key.0 as *const _));
        let pid = control2.GetProcessId().unwrap_or(0);
        // Our own sessions (sidetone) are never an app's stream.
        if pid == own_pid || key.is_empty() {
          continue;
        }
        seen.insert(key.clone());
        if let Some(s) = self.sessions.get_mut(&key) {
          // Re-read: the user may have dragged the app to another channel.
          s.bus = bus_for_exe(&s.exe);
          continue;
        }
        let Ok(meter) = control.cast::<IAudioMeterInformation>() else { continue };
        let path = if pid == 0 { String::new() } else { get_exe_path_from_pid(pid).unwrap_or_default() };
        let exe = exe_name_of(&path).to_string();
        let bus = bus_for_exe(&exe);
        self.sessions.insert(key, Session { pid, path, exe, bus, meter, history: PeakHistory::default() });
      }
      self.sessions.retain(|key, _| seen.contains(key));
    }
    Ok(())
  }

  /// Pair streams that play audio and write every live slot's bus.
  fn evaluate(&mut self) {
    let Some(apo) = self.apo.as_ref() else { return };
    let Some(now) = now_block() else { return };
    let first = now.saturating_sub((MATCH_BLOCKS + HOLD_PAD_AFTER) as u64);
    let table = &apo.layout().streams;
    let owners: Vec<(usize, u64)> = table
      .slots
      .iter()
      .enumerate()
      .map(|(i, s)| (i, s.owner.load(Ordering::Acquire)))
      .filter(|(_, o)| *o != 0)
      .collect();
    // The newest instance belongs to the running audiodg; other pids are stale slots.
    let audiodg = owners.iter().max_by_key(|(_, o)| *o as u32).map(|(_, o)| (*o >> 32) as u32);
    let meters: Vec<(&String, Vec<f32>)> = self.sessions.iter().map(|(k, s)| (k, s.history.window(first))).collect();
    let mut envelope = vec![0.0f32; MATCH_BLOCKS + HOLD_PAD_BEFORE + HOLD_PAD_AFTER];
    let mut live = HashSet::new();

    for (index, owner) in owners {
      let Some(slot) = table.slots.get(index) else { continue };
      if Some((owner >> 32) as u32) != audiodg {
        continue;
      }
      live.insert(owner);
      slot.envelope(first.saturating_sub(HOLD_PAD_BEFORE as u64), &mut envelope);
      if envelope.iter().any(|v| *v > SILENCE) {
        let matches: Vec<(&String, _)> =
          meters.iter().filter_map(|(key, meter)| hold_match(&envelope, meter).map(|m| (*key, m))).collect();
        if let Some((key, m, runner_up)) = pick_session(&matches)
          && self.pairs.get(&owner) != Some(key)
        {
          if let Some(s) = self.sessions.get(key) {
            info!(
              "[StreamRouter] stream #{} -> {} (pid {}, {:?}): {:.0} % match, runner-up {:.0} %",
              owner as u32,
              if s.exe.is_empty() { "system sounds" } else { &s.exe },
              s.pid,
              s.bus,
              m.score * 100.0,
              runner_up * 100.0
            );
          }
          self.pairs.insert(owner, key.clone());
        }
      }
      let bus = self.pairs.get(&owner).and_then(|k| self.sessions.get(k)).map_or(StreamBus::None, |s| s.bus);
      slot.bus.store(bus.to_u32(), Ordering::Relaxed);
    }
    self.pairs.retain(|owner, _| live.contains(owner));
    let paired: Vec<PairedStream> = self
      .pairs
      .iter()
      .filter_map(|(owner, key)| self.sessions.get(key).map(|s| (*owner, s)))
      .filter(|(_, s)| matches!(s.bus, StreamBus::Chat | StreamBus::Game))
      .map(|(owner, s)| PairedStream {
        owner,
        is_game: s.bus == StreamBus::Game,
        exe: s.exe.clone(),
        path: s.path.clone(),
      })
      .collect();
    if let Ok(mut p) = PAIRED.lock() {
      *p = paired;
    }
  }
}

impl Default for PeakHistory {
  fn default() -> Self {
    // u64::MAX never matches a real block, so a fresh history reads as silence.
    Self { peaks: [0.0; HISTORY], blocks: [u64::MAX; HISTORY] }
  }
}

impl PeakHistory {
  fn record(&mut self, block: u64, peak: f32) {
    let i = (block % HISTORY as u64) as usize;
    if let (Some(b), Some(p)) = (self.blocks.get_mut(i), self.peaks.get_mut(i)) {
      if *b == block {
        *p = p.max(peak);
      } else {
        (*b, *p) = (block, peak);
      }
    }
  }

  /// Peaks of blocks `first..first + MATCH_BLOCKS`; unsampled blocks read as 0.
  fn window(&self, first: u64) -> Vec<f32> {
    (first..first + MATCH_BLOCKS as u64)
      .map(|block| {
        let i = (block % HISTORY as u64) as usize;
        match (self.blocks.get(i), self.peaks.get(i)) {
          (Some(b), Some(p)) if *b == block => *p,
          _ => 0.0,
        }
      })
      .collect()
  }
}

/// Current QPC time in envelope blocks (same clock the APO stamps with).
fn now_block() -> Option<u64> {
  Some(now_hns()? / ENVELOPE_BLOCK_HNS)
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn exe_lists_decide_the_channel() {
    assert_eq!(bus_for_exe("Discord.exe"), StreamBus::Chat);
    assert_eq!(bus_for_exe("brave.exe"), StreamBus::Media);
    assert_eq!(bus_for_exe("steamwebhelper.exe"), StreamBus::Media, "a launcher is no game");
    assert_eq!(bus_for_exe("NoSuchGame.exe"), StreamBus::Media, "not recognized as a game: Media");
    crate::win_audio::app_channels::set_detected_game("TheDivision2.exe", true);
    assert_eq!(bus_for_exe("thedivision2.exe"), StreamBus::Game);
    crate::win_audio::app_channels::set_detected_game("TheDivision2.exe", false);
    assert_eq!(bus_for_exe("thedivision2.exe"), StreamBus::Media);
    assert_eq!(bus_for_exe(""), StreamBus::Media);
  }

  #[test]
  fn peak_history_keeps_block_maxima_and_reads_gaps_as_silence() {
    let mut h = PeakHistory::default();
    assert!(h.window(0).iter().all(|p| *p == 0.0), "fresh history is silent");
    h.record(1_000, 0.2);
    h.record(1_000, 0.5);
    h.record(1_002, 0.1);
    let w = h.window(1_000);
    assert_eq!((w[0], w[1], w[2]), (0.5, 0.0, 0.1));
    // A block HISTORY later reuses the index: the old value is gone, not read as current.
    h.record(1_000 + HISTORY as u64, 0.3);
    assert_eq!(h.window(1_000)[0], 0.0);
    assert_eq!(h.window(1_000 + HISTORY as u64)[0], 0.3);
  }
}
