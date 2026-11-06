//! Link to the capture APO running inside audiodg (session 0). The APO creates the Global
//! mapping `APO_SHM_NAME`; this side opens it when it appears and mirrors:
//! speaker reference + GUI settings in, AEC/denoise telemetry back into the daemon's mapping
//! (which the UI already reads). Driven from the loopback thread.
//!
//! It also persists the echo delay for the APO path: the APO runs as LOCAL SERVICE and can't
//! read `%APPDATA%`, so the cached delay of the (default mic, reference output) pair is handed
//! over in `aec_cached_delay_samples`, and a delay the APO confirms is saved back.
//! Shortcut: the key uses the *default* microphone; the APO's endpoint isn't reported yet.

use log::info;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gamersequalizer_common::shm::{AecState, StereoFrame, APO_SHM_NAME};
use gamersequalizer_mic_vst::shared_memory::SharedMemoryClient;

use super::aec_delay_cache;
use super::dsp_watch::DspWatch;
use super::loopback::resolve_default_capture_id;

/// How often to look for the APO's mapping while it doesn't exist (cheap OpenFileMapping).
const OPEN_RETRY: Duration = Duration::from_secs(2);
const SYNC_INTERVAL: Duration = Duration::from_millis(100);
/// How often to re-resolve the default microphone for the cache key (it may change, e.g. a headset).
const KEY_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// Save again only if the confirmed delay moved by more than 1 ms.
const DELAY_SAVE_MIN_CHANGE: usize = 48;

pub struct ApoLink {
  shm: Option<SharedMemoryClient>,
  last_open_try: Option<Instant>,
  last_sync: Instant,
  last_apo_seq: u32,
  render_id: Option<String>,
  cache_path: PathBuf,
  cache_key: Option<String>,
  last_key_check: Option<Instant>,
  saved_delay: Option<usize>,
  watch: DspWatch,
}

impl Default for ApoLink {
  fn default() -> Self {
    Self::new()
  }
}

impl ApoLink {
  pub fn new() -> Self {
    Self {
      shm: None,
      last_open_try: None,
      last_sync: Instant::now(),
      last_apo_seq: 0,
      render_id: None,
      cache_path: aec_delay_cache::default_path(),
      cache_key: None,
      last_key_check: None,
      saved_delay: None,
      watch: DspWatch::default(),
    }
  }

  /// The output the reference is captured from (part of the cache key).
  pub fn set_render_device(&mut self, render_id: Option<String>) {
    if render_id != self.render_id {
      self.render_id = render_id;
      self.last_key_check = None;
    }
  }

  /// Same frames the daemon's own ring got.
  pub fn push_reference(&mut self, frames: &[StereoFrame], qpc_first_hns: u64) {
    if let Some(shm) = self.shm.as_mut() {
      shm.ring_buffer().push_frames_timed(frames, qpc_first_hns);
    }
  }

  pub fn set_tap_active(&self, active: bool) {
    if let Some(shm) = self.shm.as_ref() {
      shm.layout().header.tap_active.store(active, Ordering::Relaxed);
    }
  }

  /// Some app records through the mic filter (APO or bridge), so the echo canceller needs a reference.
  pub fn mic_dsp_live(&self) -> bool {
    self.watch.is_live()
  }

  /// Open the mapping if needed, then mirror settings in and telemetry out.
  pub fn tick(&mut self, local: &SharedMemoryClient, tap_active: bool) {
    if self.last_sync.elapsed() < SYNC_INTERVAL {
      return;
    }
    self.last_sync = Instant::now();
    self.sync(local, tap_active);
    // After mirroring: the local mapping now holds the APO's or the bridge's telemetry.
    self.watch.check(&local.layout().header);
  }

  fn sync(&mut self, local: &SharedMemoryClient, tap_active: bool) {
    if self.shm.is_none() && self.last_open_try.is_none_or(|t| t.elapsed() >= OPEN_RETRY) {
      self.last_open_try = Some(Instant::now());
      // Fails until the APO has run once since boot / audio restart: expected, not logged.
      if let Ok(shm) = SharedMemoryClient::open_existing(APO_SHM_NAME) {
        info!("[ApoLink] connected to the microphone APO");
        self.last_apo_seq = shm.layout().header.aec_telemetry_seq.load(Ordering::Acquire);
        self.shm = Some(shm);
      }
    }
    let Some(apo) = self.shm.as_ref() else {
      return;
    };
    let (apo_h, local_h) = (&apo.layout().header, &local.layout().header);
    apo_h.copy_controls_from(local_h);
    apo_h.tap_active.store(tap_active, Ordering::Relaxed);
    // The output APO may run without the mic APO: its heartbeat is mirrored every tick.
    local_h.copy_output_telemetry_from(apo_h);

    // Only while the APO is producing, so a stopped APO doesn't overwrite bridge telemetry.
    let seq = apo_h.aec_telemetry_seq.load(Ordering::Acquire);
    if seq != self.last_apo_seq {
      self.last_apo_seq = seq;
      local_h.copy_dsp_telemetry_from(apo_h);
    }

    self.sync_delay_cache();
  }

  /// Hand the remembered delay to the APO; save a delay the APO measured itself.
  fn sync_delay_cache(&mut self) {
    if self.last_key_check.is_none_or(|t| t.elapsed() >= KEY_CHECK_INTERVAL) {
      self.last_key_check = Some(Instant::now());
      let key = match (resolve_default_capture_id(), self.render_id.as_deref()) {
        (Some(capture), Some(render)) => Some(aec_delay_cache::key(&capture, render)),
        _ => None,
      };
      if key != self.cache_key {
        self.saved_delay = key.as_deref().and_then(|k| aec_delay_cache::load(&self.cache_path, k));
        if let Some(d) = self.saved_delay {
          info!("[ApoLink] cached echo delay for this mic/output: {:.1} ms", d as f32 / 48.0);
        }
        self.cache_key = key;
      }
    }
    let Some(apo) = self.shm.as_ref() else {
      return;
    };
    let h = &apo.layout().header;
    h.aec_cached_delay_samples.store(self.saved_delay.unwrap_or(0) as u32, Ordering::Relaxed);

    let locked = AecState::from_u32(h.aec_state.load(Ordering::Relaxed)) == AecState::Locked;
    let delay_us = h.aec_delay_us.load(Ordering::Relaxed);
    if !locked || delay_us == 0 || h.aec_delay_from_cache.load(Ordering::Relaxed) {
      return;
    }
    let delay = (delay_us as u64 * 48 / 1000) as usize;
    if let Some(key) = self.cache_key.as_deref() {
      if self.saved_delay.is_none_or(|s| s.abs_diff(delay) > DELAY_SAVE_MIN_CHANGE) {
        info!("[ApoLink] saving echo delay {:.1} ms", delay as f32 / 48.0);
        aec_delay_cache::save(&self.cache_path, key, delay);
        self.saved_delay = Some(delay);
      }
    }
  }
}
