//! Link to the APOs running inside audiodg (session 0). The APO creates the Global mapping
//! `APO_SHM_NAME`; this side opens it when it appears and mirrors: GUI settings (and, with the
//! loopback source, the speaker reference) in, AEC/denoise/output telemetry back into the
//! daemon's mapping (which the UI already reads). Driven from the loopback thread.
//!
//! It also persists the echo delay for the APO path: the APO runs as LOCAL SERVICE and can't
//! read `%APPDATA%`, so the cached delay of the (default mic, reference output) pair is handed
//! over in `aec_cached_delay_samples`, and a delay the APO confirms is saved back.
//! Shortcut: the key uses the *default* microphone; the APO's endpoint isn't reported yet.

use log::info;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gamersequalizer_common::shm::{APO_SHM_NAME, AecState, CopiedEqVersions, StereoFrame};
use gamersequalizer_mic_chain::shared_memory::SharedMemoryClient;

use super::aec_delay_cache;
use super::console_owner::ConsoleOwner;
use super::dsp_watch::DspWatch;
use super::loopback::{RefSource, resolve_default_capture_id};

/// How often to look for the APO's mapping while it doesn't exist (cheap OpenFileMapping).
const OPEN_RETRY: Duration = Duration::from_secs(2);
const SYNC_INTERVAL: Duration = Duration::from_millis(100);
/// How often to re-resolve the default microphone for the cache key (it may change, e.g. a headset).
const KEY_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// Save again only if the confirmed delay moved by more than 1 ms.
const DELAY_SAVE_MIN_CHANGE: usize = 48;
/// Voice leveler: save the learned level when it moved by 0.5 dB, at most every 10 s.
const LEVEL_SAVE_MIN_CHANGE_X10: i32 = 5;
const LEVEL_SAVE_INTERVAL: Duration = Duration::from_secs(10);
/// Echo strength: save what the APO measured when it moved by 2 dB, at most every 10 s.
const ECHO_SAVE_MIN_CHANGE_X10: i32 = 20;
/// After "Learn again", the APO still reports the old level for a moment: don't save it back.
const LEVEL_RESET_QUIET: Duration = Duration::from_secs(2);

pub struct ApoLink {
  shm: Option<SharedMemoryClient>,
  last_open_try: Option<Instant>,
  /// Why the last open of the APO mapping failed (logged once per distinct reason).
  open_error: Option<String>,
  last_sync: Instant,
  last_apo_seq: u32,
  render_id: Option<String>,
  /// Where the reference comes from: part of the delay and echo-strength cache keys.
  source: RefSource,
  cache_path: PathBuf,
  cache_key: Option<String>,
  last_key_check: Option<Instant>,
  saved_delay: Option<usize>,
  /// Echo strength remembered for the same key (dB x 10 under the reference).
  echo_cache_path: PathBuf,
  saved_echo: Option<i32>,
  last_echo_save: Option<Instant>,
  /// Bumped on every reference output switch (`aec_reference_seq`); the APO then drops the
  /// old device's delay lock. `reference_switch_pending`: bump once the new key is loaded.
  reference_seq: u32,
  reference_switch_pending: bool,
  /// Default microphone (key of the voice level cache) and its remembered level.
  capture_id: Option<String>,
  level_cache_path: PathBuf,
  saved_level: Option<i32>,
  last_level_save: Option<Instant>,
  level_reset_seen: Option<u32>,
  level_reset_at: Option<Instant>,
  watch: DspWatch,
  console: ConsoleOwner,
  /// Last sync drove the APO; false = the next one must copy every EQ (we take over).
  driving: bool,
  /// Source EQ versions last mirrored into the APO mapping.
  copied_eqs: CopiedEqVersions,
}

impl ApoLink {
  pub fn new(source: RefSource) -> Self {
    Self {
      shm: None,
      last_open_try: None,
      open_error: None,
      last_sync: Instant::now(),
      last_apo_seq: 0,
      render_id: None,
      source,
      cache_path: aec_delay_cache::default_path(),
      cache_key: None,
      last_key_check: None,
      saved_delay: None,
      echo_cache_path: aec_delay_cache::echo_path(),
      saved_echo: None,
      last_echo_save: None,
      reference_seq: 0,
      reference_switch_pending: false,
      capture_id: None,
      level_cache_path: aec_delay_cache::leveler_path(),
      saved_level: None,
      last_level_save: None,
      level_reset_seen: None,
      level_reset_at: None,
      watch: DspWatch::default(),
      console: ConsoleOwner::default(),
      driving: false,
      copied_eqs: CopiedEqVersions::default(),
    }
  }

  /// The output the reference is captured from (part of the cache key).
  pub fn set_render_device(&mut self, render_id: Option<String>) {
    if render_id != self.render_id {
      // The first device isn't a switch: a delay the APO already locked may well be for it.
      self.reference_switch_pending |= self.render_id.is_some();
      self.render_id = render_id;
      self.last_key_check = None;
    }
  }

  /// Same frames the daemon's own ring got.
  pub fn push_reference(&mut self, frames: &[StereoFrame], qpc_first_hns: u64) {
    if !self.driving {
      return;
    }
    if let Some(shm) = self.shm.as_mut() {
      shm.ring_buffer().push_frames_timed(frames, qpc_first_hns);
    }
  }

  pub fn set_tap_active(&self, active: bool) {
    if !self.driving {
      return;
    }
    if let Some(shm) = self.shm.as_ref() {
      shm.layout().header.tap_active.store(active, Ordering::Relaxed);
    }
  }

  /// Some app records through the mic filter (the APO), so the echo canceller needs a reference.
  pub fn mic_dsp_live(&self) -> bool {
    self.watch.is_live()
  }

  /// Open the mapping if needed, then mirror settings in and telemetry out.
  pub fn tick(&mut self, local: &SharedMemoryClient, tap_active: bool) {
    if self.last_sync.elapsed() < SYNC_INTERVAL {
      return;
    }
    self.last_sync = Instant::now();
    if self.console.is_owner() {
      self.sync(local, tap_active);
    } else {
      // Another user at the console: our settings must not reach the APO (it serves them).
      self.driving = false;
    }
    // After mirroring: the local mapping now holds the APO's telemetry.
    self.watch.check(&local.layout().header);
  }

  fn sync(&mut self, local: &SharedMemoryClient, tap_active: bool) {
    if self.shm.is_none() && self.last_open_try.is_none_or(|t| t.elapsed() >= OPEN_RETRY) {
      self.last_open_try = Some(Instant::now());
      match SharedMemoryClient::open_existing(APO_SHM_NAME) {
        Ok(shm) => {
          info!("[ApoLink] connected to the microphone APO");
          self.open_error = None;
          self.last_apo_seq = shm.layout().header.aec_telemetry_seq.load(Ordering::Acquire);
          self.shm = Some(shm);
        }
        // Expected until an APO has run since boot / audio restart, so logged once per reason
        // (0x80070002: no APO created it, 0x80070005: access denied): a log then shows why it
        // never connects.
        Err(e) if self.open_error.as_deref() != Some(e.as_str()) => {
          info!("[ApoLink] APO not reachable yet: {e}");
          self.open_error = Some(e);
        }
        Err(_) => {}
      }
    }
    let Some(apo) = self.shm.as_ref() else {
      return;
    };
    let (apo_h, local_h) = (&apo.layout().header, &local.layout().header);
    if !self.driving {
      // A new owner: the APO holds another daemon's EQs, copy all of them.
      self.copied_eqs = CopiedEqVersions::default();
    }
    apo_h.copy_controls_from(local_h, &mut self.copied_eqs);
    // The APO processes only while this changes; stopping (exit, crash, user switch) = pass-through.
    apo_h.owner_beat.fetch_add(1, Ordering::Release);
    self.driving = true;
    apo_h.tap_active.store(tap_active, Ordering::Relaxed);
    // The output APO may run without the mic APO: its heartbeat is mirrored every tick.
    local_h.copy_output_telemetry_from(apo_h);

    // Only while the APO is producing: the bumped local sequence tells the watchers it runs.
    let seq = apo_h.aec_telemetry_seq.load(Ordering::Acquire);
    if seq != self.last_apo_seq {
      self.last_apo_seq = seq;
      local_h.copy_dsp_telemetry_from(apo_h);
    }

    self.sync_delay_cache();
    self.sync_level_cache(local);
  }

  /// Voice leveler: hand the level remembered for this mic to the APO, save what it learns, and
  /// forget it on "Learn again".
  fn sync_level_cache(&mut self, local: &SharedMemoryClient) {
    let reset_seq = local.layout().header.mic_leveler_reset_seq.load(Ordering::Relaxed);
    if self.level_reset_seen.is_some_and(|s| s != reset_seq) {
      if let Some(mic) = self.capture_id.as_deref() {
        info!("[ApoLink] voice leveler: learning again for this mic");
        aec_delay_cache::forget_level(&self.level_cache_path, mic);
      }
      self.saved_level = None;
      self.level_reset_at = Some(Instant::now());
    }
    self.level_reset_seen = Some(reset_seq);

    let Some(apo) = self.shm.as_ref() else {
      return;
    };
    let h = &apo.layout().header;
    h.mic_leveler_saved_level_db_x10.store(self.saved_level.unwrap_or(0), Ordering::Relaxed);

    let level = h.mic_leveler_level_db_x10.load(Ordering::Relaxed);
    let quiet = self.level_reset_at.is_some_and(|t| t.elapsed() < LEVEL_RESET_QUIET);
    let due = self.last_level_save.is_none_or(|t| t.elapsed() >= LEVEL_SAVE_INTERVAL);
    let moved = self.saved_level.is_none_or(|s| (s - level).abs() >= LEVEL_SAVE_MIN_CHANGE_X10);
    if let (Some(mic), true) = (self.capture_id.as_deref(), level != 0 && !quiet && due && moved) {
      aec_delay_cache::save_level(&self.level_cache_path, mic, level);
      self.saved_level = Some(level);
      self.last_level_save = Some(Instant::now());
    }
  }

  /// Hand the remembered delay and echo strength to the APO; save what the APO measured itself.
  fn sync_delay_cache(&mut self) {
    if self.last_key_check.is_none_or(|t| t.elapsed() >= KEY_CHECK_INTERVAL) {
      self.last_key_check = Some(Instant::now());
      let capture = resolve_default_capture_id();
      if capture != self.capture_id {
        self.saved_level = capture.as_deref().and_then(|m| aec_delay_cache::load_level(&self.level_cache_path, m));
        if let Some(l) = self.saved_level {
          info!("[ApoLink] remembered voice level for this mic: {:.1} dBFS", l as f32 / 10.0);
        }
        self.capture_id = capture.clone();
      }
      let key = match (capture, self.render_id.as_deref()) {
        (Some(capture), Some(render)) => Some(aec_delay_cache::key(&capture, render, self.source)),
        _ => None,
      };
      if key != self.cache_key {
        self.saved_delay = key.as_deref().and_then(|k| aec_delay_cache::load(&self.cache_path, k));
        if let Some(d) = self.saved_delay {
          info!("[ApoLink] cached echo delay for this mic/output: {:.1} ms", d as f32 / 48.0);
        }
        self.saved_echo = key.as_deref().and_then(|k| aec_delay_cache::load_level(&self.echo_cache_path, k));
        if let Some(e) = self.saved_echo {
          info!(
            "[ApoLink] remembered echo strength for this mic/output: {:.1} dB under the reference",
            e as f32 / 10.0
          );
        }
        self.cache_key = key;
      }
    }
    let Some(apo) = self.shm.as_ref() else {
      return;
    };
    let h = &apo.layout().header;
    h.aec_cached_delay_samples.store(self.saved_delay.unwrap_or(0) as u32, Ordering::Relaxed);
    h.aec_cached_echo_db_x10.store(self.saved_echo.unwrap_or(0), Ordering::Relaxed);
    // After the cached delay (the key was reloaded on the switch), so the APO reads both together.
    if std::mem::take(&mut self.reference_switch_pending) {
      self.reference_seq = self.reference_seq.wrapping_add(1);
      info!("[ApoLink] reference output switched: the APO measures the echo delay again");
    }
    h.aec_reference_seq.store(self.reference_seq, Ordering::Release);

    // Until the APO has handled the switch, its delay is the old output's: don't save it under
    // the new key. Acquire first, so the delay read below is at least as new as this seq.
    if h.aec_reference_seq_seen.load(Ordering::Acquire) != self.reference_seq {
      return;
    }
    let locked = AecState::from_u32(h.aec_state.load(Ordering::Relaxed)) == AecState::Locked;
    // Echo strength the APO confirmed (negative), remembered like the delay. Not a bound
    // (positive): that may be the user's voice. A remembered strength the echo no longer
    // reaches comes back lowered by the APO, as a negative value, and replaces it here.
    let echo = h.aec_echo_db_x10.load(Ordering::Relaxed);
    let moved = self.saved_echo.is_none_or(|s| (s - echo).abs() >= ECHO_SAVE_MIN_CHANGE_X10);
    let due = self.last_echo_save.is_none_or(|t| t.elapsed() >= LEVEL_SAVE_INTERVAL);
    if let (Some(key), true) = (self.cache_key.as_deref(), locked && echo < 0 && moved && due) {
      info!("[ApoLink] saving echo strength {:.1} dB under the reference", echo as f32 / 10.0);
      aec_delay_cache::save_level(&self.echo_cache_path, key, echo);
      self.saved_echo = Some(echo);
      self.last_echo_save = Some(Instant::now());
    }
    let delay_us = h.aec_delay_us.load(Ordering::Relaxed);
    if !locked || delay_us == 0 || h.aec_delay_from_cache.load(Ordering::Relaxed) {
      return;
    }
    let delay = (delay_us as u64 * 48 / 1000) as usize;
    if let Some(key) = self.cache_key.as_deref()
      && self.saved_delay.is_none_or(|s| s.abs_diff(delay) > DELAY_SAVE_MIN_CHANGE)
    {
      info!("[ApoLink] saving echo delay {:.1} ms", delay as f32 / 48.0);
      aec_delay_cache::save(&self.cache_path, key, delay);
      self.saved_delay = Some(delay);
    }
  }
}
