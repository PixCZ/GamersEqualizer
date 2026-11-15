//! Sidetone background management:
//! - Store for microphone + output pairs and their settings in `%APPDATA%\GamersEqualizer\profiles\sidetone.json`.
//! - Background worker thread, the only publisher of the current pair (device hashes, settings,
//!   EQ) into shared memory. It keeps the capture/render audio streams open when sidetone needs
//!   them and logs every change of `Verdict` (why sidetone plays or is silent).
//!   The window writes only live values of the pair the worker has published, while dragging.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime};

use gamersequalizer_common::shm::ShmHeader;
use gamersequalizer_common::sidetone::{device_hash, SidetonePairs};
use gamersequalizer_mic_chain::shared_memory::SharedMemoryClient;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Media::Audio::{
  IAudioCaptureClient, IAudioClient, IAudioClient3, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
  MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_ENGINE_PERIODICITY_LOCKED, AUDCLNT_SHAREMODE_SHARED,
  WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};

use super::loopback;
use crate::output_eq::settings::write_atomic;

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

pub const SIDETONE_FILE: &str = "sidetone.json";
const RELOAD_CHECK: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(250);
/// A beat (APO counter) that has not moved for this long means that side stopped.
pub const BEAT_FRESH: Duration = Duration::from_secs(1);
const SLEEP_ON_ERROR: Duration = Duration::from_secs(1);
/// A stream that failed to open is tried again on the same device after this long.
const OPEN_RETRY: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------------------

struct Store {
  path: Option<PathBuf>,
  modified: Option<SystemTime>,
  last_check: Option<Instant>,
  pairs: SidetonePairs,
  /// Bumped on every load and save: readers compare it to know the pairs changed.
  version: u32,
}

static STORE: RwLock<Store> = RwLock::new(Store {
  path: None,
  modified: None,
  last_check: None,
  pairs: SidetonePairs { pairs: Vec::new() },
  version: 0,
});

/// Point the store at `<base_dir>/sidetone.json` and load it. Call once per process.
pub fn init(base_dir: &Path) {
  let mut store = STORE.write().unwrap_or_else(|e| e.into_inner());
  store.path = Some(base_dir.join(SIDETONE_FILE));
  store.reload();
}

/// The saved pairs and their version (re-read from disk when the file changed).
pub fn pairs() -> (SidetonePairs, u32) {
  refresh_if_due();
  let store = STORE.read().unwrap_or_else(|e| e.into_inner());
  (store.pairs.clone(), store.version)
}

/// Replace the pairs and save at once (the window calls this).
pub fn save(pairs: SidetonePairs) {
  let mut store = STORE.write().unwrap_or_else(|e| e.into_inner());
  store.pairs = pairs;
  store.version = store.version.wrapping_add(1);
  let Some(path) = store.path.clone() else { return };
  let saved = store.pairs.to_json().map_err(std::io::Error::other).and_then(|text| write_atomic(&path, &text));
  match saved {
    Ok(()) => store.modified = modified(&path),
    Err(e) => log::error!("[Sidetone] saving {} failed: {e}", path.display()),
  }
}

fn refresh_if_due() {
  let due = {
    let store = STORE.read().unwrap_or_else(|e| e.into_inner());
    store.path.is_some() && store.last_check.is_none_or(|t| t.elapsed() >= RELOAD_CHECK)
  };
  if due {
    let mut store = STORE.write().unwrap_or_else(|e| e.into_inner());
    store.last_check = Some(Instant::now());
    let changed = store.path.as_deref().map(modified) != Some(store.modified);
    if changed {
      store.reload();
    }
  }
}

impl Store {
  fn reload(&mut self) {
    let Some(path) = self.path.clone() else { return };
    self.modified = modified(&path);
    self.pairs = match std::fs::read_to_string(&path) {
      Ok(text) => SidetonePairs::from_json(&text).unwrap_or_else(|e| {
        log::warn!("[Sidetone] {} is corrupt ({e}), ignoring it", path.display());
        crate::output_eq::settings::keep_corrupt(&path);
        SidetonePairs { pairs: Vec::new() }
      }),
      Err(e) => {
        if e.kind() != std::io::ErrorKind::NotFound {
          log::warn!("[Sidetone] reading {} failed ({e}), no pairs until it can be read", path.display());
        }
        SidetonePairs { pairs: Vec::new() }
      }
    };
    self.version = self.version.wrapping_add(1);
  }
}

fn modified(path: &Path) -> Option<SystemTime> {
  std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

// ---------------------------------------------------------------------------------------
// Pair and verdict (shared by the worker and the window)
// ---------------------------------------------------------------------------------------

/// Microphone and output ids sidetone follows: the picked microphone, else the Windows default
/// one, and the Windows default output. None while either is unknown.
pub fn active_pair(
  picked_mic: Option<String>,
  default_mic: impl FnOnce() -> Option<String>,
  default_output: Option<String>,
) -> Option<(String, String)> {
  Some((picked_mic.or_else(default_mic)?, default_output?))
}

/// Why sidetone plays or is silent, from what the two APOs report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
  Off,
  Muted,
  /// The APO of another microphone runs; the pair's microphone does not.
  OtherMicrophone,
  /// The APO of another output runs; the pair's output does not.
  OtherOutput,
  /// No app records from the microphone (and "Try it" is off).
  NoRecorder,
  Starting,
  Playing,
}

/// `enabled`: the pair is on. The `*_running` flags: that APO's beat moved within `BEAT_FRESH`.
pub fn verdict(h: &ShmHeader, enabled: bool, writer_running: bool, reader_running: bool, testing: bool) -> Verdict {
  let r = Ordering::Relaxed;
  let other = |seen: u32, wanted: u32| seen != 0 && seen != wanted;
  if !enabled {
    Verdict::Off
  } else if h.mic_muted.load(r) {
    Verdict::Muted
  } else if other(h.sidetone_mic_hash_seen.load(r), h.sidetone_mic_hash.load(r)) && !writer_running {
    Verdict::OtherMicrophone
  } else if other(h.sidetone_output_hash_seen.load(r), h.sidetone_output_hash.load(r)) && !reader_running {
    Verdict::OtherOutput
  } else if !writer_running && !testing {
    Verdict::NoRecorder
  } else if reader_running {
    Verdict::Playing
  } else {
    Verdict::Starting
  }
}

// ---------------------------------------------------------------------------------------
// Background worker thread
// ---------------------------------------------------------------------------------------

struct BeatWatch {
  last_value: u32,
  last_change: Instant,
}

impl BeatWatch {
  fn new(val: u32) -> Self {
    Self { last_value: val, last_change: Instant::now() }
  }

  fn update(&mut self, val: u32, fresh_dur: Duration) -> bool {
    let now = Instant::now();
    if val != self.last_value {
      self.last_value = val;
      self.last_change = now;
      true
    } else {
      now.duration_since(self.last_change) < fresh_dur
    }
  }
}

/// `GetMixFormat` memory, freed on drop.
struct MixFormat(*mut WAVEFORMATEX);

impl Drop for MixFormat {
  fn drop(&mut self) {
    // SAFETY: allocated by `GetMixFormat` (CoTaskMemAlloc) and freed only here; null is allowed.
    unsafe { CoTaskMemFree(Some(self.0 as *const c_void)) };
  }
}

/// An open, running stream that keeps a device's engine (and so its APO) running. `S` is the
/// capture or the render client.
struct Stream<S> {
  device_id: String,
  /// What was asked for, not what was granted: a fallback to the default period must not look
  /// like a changed wish (that reopened the stream every poll).
  low_latency: bool,
  client: IAudioClient,
  service: S,
  buffer_frames: u32,
  _format: MixFormat,
}

impl<S> Drop for Stream<S> {
  fn drop(&mut self) {
    // SAFETY: COM call on a live interface; a failure only means it already stopped.
    let _ = unsafe { self.client.Stop() };
  }
}

/// Last failed open of one stream kind: retried after `OPEN_RETRY`, logged once per device and error.
#[derive(Default)]
struct OpenFailure(Option<(String, String, Instant)>);

impl OpenFailure {
  fn may_retry(&self, id: &str) -> bool {
    self.0.as_ref().is_none_or(|(failed_id, _, at)| failed_id != id || at.elapsed() >= OPEN_RETRY)
  }

  fn record(&mut self, tag: &str, id: &str, err: &windows::core::Error) {
    let text = err.to_string();
    if self.0.as_ref().is_none_or(|(failed_id, failed_text, _)| failed_id != id || *failed_text != text) {
      log::warn!("[Sidetone] opening the {tag} stream on {id} failed: {text} (next try in {}s)", OPEN_RETRY.as_secs());
    }
    self.0 = Some((id.to_string(), text, Instant::now()));
  }
}

/// # Safety
/// COM must be initialized on this thread.
unsafe fn open_device(id: &str) -> windows::core::Result<IMMDevice> {
  unsafe {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    enumerator.GetDevice(PCWSTR(wide.as_ptr()))
  }
}

/// Opens and starts a shared-mode stream on device `id`; `tag` ("capture" / "render") names it
/// in the log. With `low_latency` it asks for the engine's shortest period and falls back to
/// the default one.
///
/// # Safety
/// COM must be initialized on this thread.
unsafe fn open_stream<S: Interface>(id: &str, low_latency: bool, tag: &str) -> windows::core::Result<Stream<S>> {
  unsafe {
    let device = open_device(id)?;
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
    let format = MixFormat(client.GetMixFormat()?);
    let granted = low_latency && init_low_latency(&client, format.0, tag);
    if low_latency && !granted {
      log::info!("[Sidetone] {tag} on {id}: shortest period not available, using the default one");
    }
    if !granted {
      client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, format.0, None)?;
    }
    let service: S = client.GetService()?;
    let buffer_frames = client.GetBufferSize()?;
    client.Start()?;
    Ok(Stream { device_id: id.to_string(), low_latency, client, service, buffer_frames, _format: format })
  }
}

/// Initializes `client` at the engine's shortest period (or the current one when another
/// stream locked it). False when that is not possible; the client is then still uninitialized.
///
/// # Safety
/// `format` is the client's mix format; COM must be initialized on this thread.
unsafe fn init_low_latency(client: &IAudioClient, format: *mut WAVEFORMATEX, tag: &str) -> bool {
  unsafe {
    let Ok(client3) = client.cast::<IAudioClient3>() else {
      return false;
    };
    let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
    if client3.GetSharedModeEnginePeriod(format, &mut default, &mut fundamental, &mut min, &mut max).is_err() {
      return false;
    }
    let Err(err) = client3.InitializeSharedAudioStream(0, min, format, None) else {
      return true;
    };
    if err.code() != AUDCLNT_E_ENGINE_PERIODICITY_LOCKED {
      log::warn!("[Sidetone] InitializeSharedAudioStream failed on {tag}: {err}");
      return false;
    }
    let mut current_format: *mut WAVEFORMATEX = std::ptr::null_mut();
    let mut current = 0u32;
    if client3.GetCurrentSharedModeEnginePeriod(&mut current_format, &mut current).is_err() {
      return false;
    }
    drop(MixFormat(current_format));
    log::info!("[Sidetone] engine periodicity locked on {tag} ({current} frames, min {min})");
    client3.InitializeSharedAudioStream(0, current, format, None).is_ok()
  }
}

/// Closes the stream in `slot` when it is not `wanted` any more (other device, other period
/// wish, not needed) and opens the wanted one, at most every `OPEN_RETRY` after a failure.
fn keep_open<S: Interface>(
  slot: &mut Option<Stream<S>>,
  failure: &mut OpenFailure,
  wanted: Option<&str>,
  low_latency: bool,
  tag: &str,
) {
  if slot.as_ref().is_some_and(|s| Some(s.device_id.as_str()) != wanted || s.low_latency != low_latency) {
    *slot = None;
  }
  let Some(id) = wanted else { return };
  if slot.is_none() && failure.may_retry(id) {
    // SAFETY: COM is initialized on the worker thread (`spawn`).
    match unsafe { open_stream::<S>(id, low_latency, tag) } {
      Ok(stream) => {
        *slot = Some(stream);
        failure.0 = None;
      }
      Err(e) => failure.record(tag, id, &e),
    }
  }
}

struct WorkerState {
  shm: SharedMemoryClient,
  last_pair: Option<(String, String)>,
  last_version: u32,
  test_watch: BeatWatch,
  writer_watch: BeatWatch,
  reader_watch: BeatWatch,
  last_verdict: Option<Verdict>,
  render_stream: Option<Stream<IAudioRenderClient>>,
  capture_stream: Option<Stream<IAudioCaptureClient>>,
  render_failure: OpenFailure,
  capture_failure: OpenFailure,
}

impl WorkerState {
  fn new(shm: SharedMemoryClient) -> Self {
    let h = &shm.layout().header;
    let r = Ordering::Relaxed;
    let test_watch = BeatWatch::new(h.sidetone_test_beat.load(r));
    let writer_watch = BeatWatch::new(h.sidetone_writer_beat.load(r));
    let reader_watch = BeatWatch::new(h.sidetone_reader_beat.load(r));
    Self {
      shm,
      last_pair: None,
      // The store starts at version 1 or more (`init` loads it): the first poll publishes.
      last_version: 0,
      test_watch,
      writer_watch,
      reader_watch,
      last_verdict: None,
      render_stream: None,
      capture_stream: None,
      render_failure: OpenFailure::default(),
      capture_failure: OpenFailure::default(),
    }
  }

  fn iteration(&mut self) -> windows::core::Result<()> {
    let h = &self.shm.layout().header;
    let r = Ordering::Relaxed;

    // 1. The pair, and publishing it when it or the saved pairs changed. The only writer of
    // the hashes and of `enabled`; hashes, EQ and values before `enabled` (`write_sidetone`).
    let pair_ids =
      active_pair(h.get_selected_mic_id(), loopback::resolve_default_capture_id, loopback::resolve_render_id(None));
    let (current_pairs, version) = pairs();
    if pair_ids != self.last_pair || version != self.last_version {
      self.last_version = version;
      let pair = pair_ids.as_ref().and_then(|(m, o)| current_pairs.find(m, o));
      let settings = pair.map(|p| p.settings()).unwrap_or_default();
      let hashes = match &pair_ids {
        Some((m, o)) if settings.enabled => (device_hash(m), device_hash(o)),
        _ => (0, 0),
      };
      // Off while the pair changes, so no APO runs the new devices with the old values. Not on a
      // plain save (a slider released): that would cut the sidetone for a moment.
      if pair_ids != self.last_pair {
        h.sidetone_enabled.store(false, Ordering::Release);
        self.last_pair = pair_ids.clone();
      }
      h.sidetone_mic_hash.store(hashes.0, r);
      h.sidetone_output_hash.store(hashes.1, r);
      let bands = pair.map_or(&[][..], |p| p.bands.as_slice());
      h.write_sidetone_eq(bands);
      h.write_sidetone(&settings);
      let (mic, out) = pair_ids.as_ref().map_or(("none", "none"), |(m, o)| (m.as_str(), o.as_str()));
      log::info!(
        "[Sidetone] pair {mic} -> {out}: {}, volume {:.0} % of the leveled voice, {} EQ bands",
        if settings.enabled { "on" } else { "off" },
        settings.volume * 100.0,
        bands.len()
      );
    }

    // 2. Beats, and why it plays or not (logged on every change: the tester's log must say).
    let testing = self.test_watch.update(h.sidetone_test_beat.load(r), BEAT_FRESH);
    let feeding = self.writer_watch.update(h.sidetone_writer_beat.load(r), BEAT_FRESH);
    let playing = self.reader_watch.update(h.sidetone_reader_beat.load(r), BEAT_FRESH);
    let enabled = h.read_sidetone().enabled;
    let verdict = verdict(h, enabled, feeding, playing, testing);
    if self.last_verdict != Some(verdict) {
      self.last_verdict = Some(verdict);
      log::info!(
        "[Sidetone] {verdict:?} (mic APO {:08x}/{:08x}, output APO {:08x}/{:08x} seen/wanted, dropouts {}, resyncs {})",
        h.sidetone_mic_hash_seen.load(r),
        h.sidetone_mic_hash.load(r),
        h.sidetone_output_hash_seen.load(r),
        h.sidetone_output_hash.load(r),
        h.sidetone_underruns.load(r),
        h.sidetone_resyncs.load(r),
      );
    }

    // 3. Keep the devices running while sidetone needs them.
    let low_latency = h.sidetone_low_latency.load(r);
    let keep_render = enabled && (testing || feeding);
    let keep_capture = enabled && (testing || (low_latency && crate::mic_usage::microphone_in_use()));
    let (mic_id, out_id) = pair_ids.as_ref().map_or((None, None), |(m, o)| (Some(m.as_str()), Some(o.as_str())));
    let wanted_render = out_id.filter(|_| keep_render);
    let wanted_capture = mic_id.filter(|_| keep_capture);
    keep_open(&mut self.render_stream, &mut self.render_failure, wanted_render, low_latency, "render");
    keep_open(&mut self.capture_stream, &mut self.capture_failure, wanted_capture, low_latency, "capture");

    if let Some(stream) = self.render_stream.as_ref() {
      // SAFETY: COM calls on live interfaces of a started stream; silence is written as a flag,
      // the buffer itself is never touched.
      unsafe {
        let padding = stream.client.GetCurrentPadding()?;
        let free = stream.buffer_frames.saturating_sub(padding);
        if free > 0 {
          stream.service.GetBuffer(free)?;
          stream.service.ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
        }
      }
    }
    if let Some(stream) = self.capture_stream.as_ref() {
      // SAFETY: COM calls on live interfaces of a started stream; every packet is released
      // unread right after it is taken.
      unsafe {
        while stream.service.GetNextPacketSize()? > 0 {
          let mut data = std::ptr::null_mut();
          let mut frames = 0u32;
          let mut flags = 0u32;
          stream.service.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
          stream.service.ReleaseBuffer(frames)?;
        }
      }
    }

    Ok(())
  }
}

/// Runs for the daemon's life: publishes the current pair's settings and keeps the devices
/// running while sidetone needs them.
pub fn spawn() -> Option<std::thread::JoinHandle<()>> {
  std::thread::Builder::new()
    .name("GamersEqualizer_Sidetone".into())
    .spawn(|| {
      // SAFETY: once, on this new thread, before any COM call.
      let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
      if com.is_err() {
        log::warn!("[Sidetone] CoInitializeEx: {com:?}");
      }
      log::info!("[Sidetone] started");
      let shm = match SharedMemoryClient::open_or_create() {
        Ok(c) => c,
        Err(e) => {
          log::error!("[Sidetone] shared memory unavailable: {e}");
          return;
        }
      };
      let mut worker = WorkerState::new(shm);
      loop {
        if let Err(e) = worker.iteration() {
          log::warn!("[Sidetone] iteration failed: {e}");
          worker.render_stream = None;
          worker.capture_stream = None;
          std::thread::sleep(SLEEP_ON_ERROR);
        } else {
          std::thread::sleep(POLL);
        }
      }
    })
    .ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
  use super::*;
  use gamersequalizer_common::sidetone::SidetonePair;

  #[test]
  fn verdict_names_the_first_reason_and_pair_needs_both_devices() {
    let h = ShmHeader::default();
    let r = Ordering::Relaxed;
    assert_eq!(verdict(&h, false, true, true, false), Verdict::Off);
    h.sidetone_mic_hash.store(11, r);
    h.sidetone_output_hash.store(22, r);
    h.sidetone_output_hash_seen.store(33, r);
    assert_eq!(verdict(&h, true, true, false, false), Verdict::OtherOutput);
    h.sidetone_output_hash_seen.store(22, r);
    assert_eq!(verdict(&h, true, false, false, false), Verdict::NoRecorder);
    assert_eq!(verdict(&h, true, false, false, true), Verdict::Starting, "Try it feeds without a recorder");
    assert_eq!(verdict(&h, true, true, true, false), Verdict::Playing);
    h.mic_muted.store(true, r);
    assert_eq!(verdict(&h, true, true, true, false), Verdict::Muted);

    let picked = Some("mic".to_string());
    assert_eq!(active_pair(None, || None, Some("out".into())), None);
    assert_eq!(active_pair(picked.clone(), || None, None), None);
    assert_eq!(active_pair(picked, || Some("default".into()), Some("out".into())), Some(("mic".into(), "out".into())));
  }

  #[test]
  fn test_sidetone_store_save_and_corrupt() {
    let dir = std::env::temp_dir().join(format!("ge_sidetone_test_{}", std::process::id()));
    init(&dir);
    let mut p = SidetonePairs { pairs: Vec::new() };
    p.upsert(SidetonePair::new("mic1", "out1", "Mic 1", "Out 1"));
    p.upsert(SidetonePair::new("mic2", "out2", "Mic 2", "Out 2"));
    save(p);

    let (loaded, _) = pairs();
    assert_eq!(loaded.pairs.len(), 2);
    assert!(loaded.find("mic1", "out1").is_some());

    // Corrupt the file
    let path = dir.join(SIDETONE_FILE);
    std::fs::write(&path, "corrupt json text").unwrap();
    let mut store = STORE.write().unwrap();
    store.reload();
    drop(store);

    let (after_corrupt, _) = pairs();
    assert_eq!(after_corrupt.pairs.len(), 0);
    let _ = std::fs::remove_dir_all(&dir);
  }
}
