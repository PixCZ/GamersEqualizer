//! Sidetone background management:
//! - Store for microphone + output pairs and their settings in `%APPDATA%\GamersEqualizer\profiles\sidetone.json`.
//! - Background worker thread that syncs the current pair's settings into shared memory and
//!   keeps the capture/render audio streams open when sidetone needs them.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime};

use gamersequalizer_common::sidetone::{device_hash, SidetonePairs, SidetoneSettings};
use gamersequalizer_mic_vst::shared_memory::SharedMemoryClient;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Media::Audio::{
  IAudioCaptureClient, IAudioClient, IAudioClient3, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
  MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_ENGINE_PERIODICITY_LOCKED, AUDCLNT_SHAREMODE_SHARED,
  WAVEFORMATEX,
};
use windows::Win32::System::Com::{
  CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use crate::output_eq::settings::write_atomic;
use super::loopback;

// ---------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------

pub const SIDETONE_FILE: &str = "sidetone.json";
const RELOAD_CHECK: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(250);
const BEAT_FRESH: Duration = Duration::from_secs(1);
const SLEEP_ON_ERROR: Duration = Duration::from_secs(1);

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
  let saved = store
    .pairs
    .to_json()
    .map_err(std::io::Error::other)
    .and_then(|text| write_atomic(&path, &text));
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
      Err(_) => SidetonePairs { pairs: Vec::new() },
    };
    self.version = self.version.wrapping_add(1);
  }
}

fn modified(path: &Path) -> Option<SystemTime> {
  std::fs::metadata(path).and_then(|m| m.modified()).ok()
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

struct CaptureStream {
  device_id: String,
  client: IAudioClient,
  capture: IAudioCaptureClient,
  low_latency: bool,
  format: *mut WAVEFORMATEX,
}

impl Drop for CaptureStream {
  fn drop(&mut self) {
    let _ = unsafe { self.client.Stop() };
    if !self.format.is_null() {
      unsafe { CoTaskMemFree(Some(self.format as *const std::ffi::c_void)); }
    }
  }
}

struct RenderStream {
  device_id: String,
  client: IAudioClient,
  render: IAudioRenderClient,
  buffer_frames: u32,
  low_latency: bool,
  format: *mut WAVEFORMATEX,
}

impl Drop for RenderStream {
  fn drop(&mut self) {
    let _ = unsafe { self.client.Stop() };
    if !self.format.is_null() {
      unsafe { CoTaskMemFree(Some(self.format as *const std::ffi::c_void)); }
    }
  }
}

unsafe fn open_device(id: &str) -> windows::core::Result<IMMDevice> {
  let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
  let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
  enumerator.GetDevice(PCWSTR(wide.as_ptr()))
}

unsafe fn open_capture(id: &str, low_latency: bool) -> windows::core::Result<CaptureStream> {
  let device = open_device(id)?;
  let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
  let format = client.GetMixFormat()?;
  let mut used_low_latency = false;
  if low_latency {
    if let Ok(client3) = client.cast::<IAudioClient3>() {
      let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
      if client3.GetSharedModeEnginePeriod(format, &mut default, &mut fundamental, &mut min, &mut max).is_ok() {
        let res = client3.InitializeSharedAudioStream(0, min, format, None);
        if let Err(err) = res {
          if err.code() == AUDCLNT_E_ENGINE_PERIODICITY_LOCKED {
            let mut cur_fmt: *mut WAVEFORMATEX = std::ptr::null_mut();
            let mut cur_p = 0u32;
            if client3.GetCurrentSharedModeEnginePeriod(&mut cur_fmt, &mut cur_p).is_ok() {
              if !cur_fmt.is_null() {
                CoTaskMemFree(Some(cur_fmt as *const std::ffi::c_void));
              }
              log::info!("[Sidetone] engine periodicity locked on capture ({cur_p} frames, min {min})");
              if client3.InitializeSharedAudioStream(0, cur_p, format, None).is_ok() {
                used_low_latency = true;
              }
            }
          } else {
            log::warn!("[Sidetone] InitializeSharedAudioStream failed on capture: {err}");
          }
        } else {
          used_low_latency = true;
        }
      }
    }
  }
  if !used_low_latency {
    client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, format, None)?;
  }
  let capture: IAudioCaptureClient = client.GetService()?;
  client.Start()?;
  Ok(CaptureStream {
    device_id: id.to_string(),
    client,
    capture,
    low_latency: used_low_latency,
    format,
  })
}

unsafe fn open_render(id: &str, low_latency: bool) -> windows::core::Result<RenderStream> {
  let device = open_device(id)?;
  let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
  let format = client.GetMixFormat()?;
  let mut used_low_latency = false;
  if low_latency {
    if let Ok(client3) = client.cast::<IAudioClient3>() {
      let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
      if client3.GetSharedModeEnginePeriod(format, &mut default, &mut fundamental, &mut min, &mut max).is_ok() {
        let res = client3.InitializeSharedAudioStream(0, min, format, None);
        if let Err(err) = res {
          if err.code() == AUDCLNT_E_ENGINE_PERIODICITY_LOCKED {
            let mut cur_fmt: *mut WAVEFORMATEX = std::ptr::null_mut();
            let mut cur_p = 0u32;
            if client3.GetCurrentSharedModeEnginePeriod(&mut cur_fmt, &mut cur_p).is_ok() {
              if !cur_fmt.is_null() {
                CoTaskMemFree(Some(cur_fmt as *const std::ffi::c_void));
              }
              log::info!("[Sidetone] engine periodicity locked on render ({cur_p} frames, min {min})");
              if client3.InitializeSharedAudioStream(0, cur_p, format, None).is_ok() {
                used_low_latency = true;
              }
            }
          } else {
            log::warn!("[Sidetone] InitializeSharedAudioStream failed on render: {err}");
          }
        } else {
          used_low_latency = true;
        }
      }
    }
  }
  if !used_low_latency {
    client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, format, None)?;
  }
  let render: IAudioRenderClient = client.GetService()?;
  let buffer_frames = client.GetBufferSize()?;
  client.Start()?;
  Ok(RenderStream {
    device_id: id.to_string(),
    client,
    render,
    buffer_frames,
    low_latency: used_low_latency,
    format,
  })
}

struct WorkerState {
  shm: SharedMemoryClient,
  last_mic_id: Option<String>,
  last_out_id: Option<String>,
  last_version: u32,
  test_watch: BeatWatch,
  writer_watch: BeatWatch,
  render_stream: Option<RenderStream>,
  capture_stream: Option<CaptureStream>,
}

impl WorkerState {
  fn new(shm: SharedMemoryClient) -> Self {
    let (test_b, writer_b) = {
      let h = &shm.layout().header;
      let r = Ordering::Relaxed;
      (h.sidetone_test_beat.load(r), h.sidetone_writer_beat.load(r))
    };
    Self {
      shm,
      last_mic_id: None,
      last_out_id: None,
      last_version: 0,
      test_watch: BeatWatch::new(test_b),
      writer_watch: BeatWatch::new(writer_b),
      render_stream: None,
      capture_stream: None,
    }
  }

  fn iteration(&mut self) -> windows::core::Result<()> {
    let h = &self.shm.layout().header;
    let r = Ordering::Relaxed;

    // 1. Dvojice
    let mic_id = h.get_selected_mic_id().or_else(loopback::resolve_default_capture_id);
    let out_id = loopback::resolve_render_id(None);

    // 2. Nastavení
    let (current_pairs, version) = pairs();
    let changed = mic_id != self.last_mic_id || out_id != self.last_out_id || version != self.last_version;
    if changed {
      self.last_mic_id = mic_id.clone();
      self.last_out_id = out_id.clone();
      self.last_version = version;

      let settings = match (&mic_id, &out_id) {
        (Some(m), Some(o)) => current_pairs.find(m, o).map(|p| p.settings()).unwrap_or_default(),
        _ => SidetoneSettings::default(),
      };

      if settings.enabled {
        let m_hash = mic_id.as_deref().map(device_hash).unwrap_or(0);
        let o_hash = out_id.as_deref().map(device_hash).unwrap_or(0);
        h.sidetone_mic_hash.store(m_hash, r);
        h.sidetone_output_hash.store(o_hash, r);
      } else {
        h.sidetone_mic_hash.store(0, r);
        h.sidetone_output_hash.store(0, r);
      }
      h.write_sidetone(&settings);
      log::info!(
        "[Sidetone] pair {} -> {}: {}",
        mic_id.as_deref().unwrap_or("none"),
        out_id.as_deref().unwrap_or("none"),
        if settings.enabled { "on" } else { "off" }
      );
    }

    // 3. Tepy
    let testing = self.test_watch.update(h.sidetone_test_beat.load(r), BEAT_FRESH);
    let feeding = self.writer_watch.update(h.sidetone_writer_beat.load(r), BEAT_FRESH);

    // 4. Co držet otevřené
    let sidetone_settings = h.read_sidetone();
    let enabled = sidetone_settings.enabled;
    let low_latency = h.sidetone_low_latency.load(r);

    let should_keep_render = enabled && (testing || feeding);
    let should_keep_capture = enabled && (testing || (low_latency && crate::mic_usage::microphone_in_use()));

    // 5. Srovnat skutečnost
    // Render stream
    if let Some(stream) = self.render_stream.as_ref() {
      let wanted_id = out_id.as_deref().unwrap_or_default();
      if !should_keep_render || stream.device_id != wanted_id || stream.low_latency != low_latency {
        self.render_stream = None;
      }
    }
    if should_keep_render && self.render_stream.is_none() {
      if let Some(id) = out_id.as_deref() {
        unsafe {
          match open_render(id, low_latency) {
            Ok(s) => self.render_stream = Some(s),
            Err(e) => log::warn!("[Sidetone] open_render on {id} failed: {e}"),
          }
        }
      }
    }
    if let Some(stream) = self.render_stream.as_mut() {
      unsafe {
        let padding = stream.client.GetCurrentPadding()?;
        let free = stream.buffer_frames.saturating_sub(padding);
        if free > 0 {
          stream.render.GetBuffer(free)?;
          stream.render.ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
        }
      }
    }

    // Capture stream
    if let Some(stream) = self.capture_stream.as_ref() {
      let wanted_id = mic_id.as_deref().unwrap_or_default();
      if !should_keep_capture || stream.device_id != wanted_id || stream.low_latency != low_latency {
        self.capture_stream = None;
      }
    }
    if should_keep_capture && self.capture_stream.is_none() {
      if let Some(id) = mic_id.as_deref() {
        unsafe {
          match open_capture(id, low_latency) {
            Ok(s) => self.capture_stream = Some(s),
            Err(e) => log::warn!("[Sidetone] open_capture on {id} failed: {e}"),
          }
        }
      }
    }
    if let Some(stream) = self.capture_stream.as_mut() {
      unsafe {
        while stream.capture.GetNextPacketSize()? > 0 {
          let mut data = std::ptr::null_mut();
          let mut frames = 0u32;
          let mut flags = 0u32;
          stream.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
          stream.capture.ReleaseBuffer(frames)?;
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
      unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
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
