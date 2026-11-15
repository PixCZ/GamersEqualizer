pub use crate::output_eq::{OutputEq, OutputLayer};
use crate::sonar_importer::ProfileStorage;
use crate::win_audio::{AudioEndpointInfo, ChatMixController, SpatialAudioStatus, SpatialFormat};
use gamersequalizer_common::biquad::MAX_EQ_BANDS;
use gamersequalizer_common::profile::{ParametricProfile, VadType, collapse_slopes, slots_used};
use gamersequalizer_common::shm::{APO_SHM_NAME, AecState, DenoiseLimit, DenoiseModel, SHM_NAME, ShmHeader, ShmLayout};
use gamersequalizer_common::sidetone::SidetonePair;
use gamersequalizer_common::streams::StreamBus;
use gamersequalizer_mic_chain::shared_memory::SharedMemoryClient;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
  CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::core::PCWSTR;

/// File holding the microphone EQ the user is currently editing.
const MIC_EQ_FILE: &str = "mic_active.json";
/// AEC history kept for the plots (~60 s at 10 updates/s).
const AEC_HISTORY_LEN: usize = 600;
/// Mic DSP counts as running while its telemetry changed within this window (it publishes ~10x/s).
const MIC_DSP_LIVE_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
/// Time a model switch (or stream start) may take before a mismatch counts as a failed load.
const DENOISE_SWITCH_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// One snapshot of the echo canceller's telemetry, read from shared memory.
#[derive(Debug, Clone, Default)]
pub struct AecTelemetry {
  pub state: Option<AecState>,
  pub delay_ms: Option<f32>,
  pub raw_delay_ms: Option<f32>,
  pub from_cache: bool,
  pub confidence: f32,
  pub estimate_count: u32,
  pub erle_db: f32,
  pub double_talk: bool,
  pub ref_db: f32,
  pub mic_db: f32,
  pub out_db: f32,
  pub filter_taps: u32,
  pub pre_delay_ms: f32,
  pub corr_max_delay_ms: f32,
  pub impulse: Vec<f32>,
  pub corr: Vec<f32>,
  pub seq: u32,
  /// A mic stream published telemetry recently. False = no app uses the mic, so the values
  /// (including `state`) are left over from the last stream or still zero.
  pub live: bool,
}

/// Noise suppression as the UI should present it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenoiseStatus {
  /// No app is using the microphone: nothing runs; this model starts with the next stream.
  Idle(DenoiseModel),
  Running(DenoiseModel),
  /// Selection changed or stream started moments ago; this one still runs.
  Switching(DenoiseModel),
  /// The DSP reports why the selected model doesn't run (load failed / CPU too slow).
  Limited {
    limit: DenoiseLimit,
    running: DenoiseModel,
  },
  /// Mismatch without a reason from the DSP (e.g. an older APO build).
  Failed {
    selected: DenoiseModel,
    running: DenoiseModel,
  },
}

/// Delay the mic chain adds, ms (see `mic_latency`).
#[derive(Debug, Clone, Copy, Default)]
pub struct MicLatency {
  /// None until the DSP has heard enough speech to measure.
  pub measured_ms: Option<f32>,
  pub aec_ms: f32,
  pub denoise_ms: f32,
  pub lookahead_ms: f32,
}

/// Mic chain CPU time as a share of real time (1.0 = 100 %), last ~100 ms.
#[derive(Debug, Clone, Copy, Default)]
pub struct DspLoad {
  pub avg: f32,
  pub peak: f32,
  /// Late blocks since the stream started.
  pub late_blocks: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct AecHistoryPoint {
  pub t: f64,
  pub erle_db: f32,
  pub delay_ms: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActiveTab {
  Mixer,
  Headphones,
  Game,
  Chat,
  Media,
  Sidetone,
  Mic,
  Log,
  Settings,
}

#[derive(Default)]
pub struct SidetoneUi {
  pub testing: bool,
  pub pair: Option<SidetonePair>,
  pub pair_key: (String, String),
  pub dirty: bool,
  /// (mic id, output id) of the saved pair whose Delete waits for confirmation.
  pub confirm_delete: Option<(String, String)>,
  pub last_reader_beat: u32,
  pub last_reader_beat_time: Option<std::time::Instant>,
  pub last_writer_beat: u32,
  pub last_writer_beat_time: Option<std::time::Instant>,
}

/// How long the double-talk indicator stays visible after the last detection.
const DOUBLE_TALK_HOLD: std::time::Duration = std::time::Duration::from_millis(1500);

pub struct AppState {
  pub active_tab: ActiveTab,
  pub endpoints: Vec<AudioEndpointInfo>,
  pub capture_endpoints: Vec<AudioEndpointInfo>,
  pub selected_endpoint_id: Option<String>,
  pub selected_chat_endpoint_id: Option<String>,
  pub selected_mic_endpoint_id: Option<String>,
  pub spatial_status: SpatialAudioStatus,
  /// Format running on the default output, and the one the user has there (health memory).
  pub spatial_format: Option<SpatialFormat>,
  pub spatial_expected: Option<SpatialFormat>,
  /// The default output both were read on.
  pub spatial_output_id: Option<String>,
  /// None = check on the next frame.
  pub spatial_checked_at: Option<std::time::Instant>,
  pub active_game_name: Option<String>,
  pub autostart_enabled: bool,
  /// Language and theme (also read by the background service for the tray menu).
  pub prefs: super::prefs::UiPrefs,

  /// Headphone correction + game profile on the output, master switch.
  pub output_eq: OutputEq,
  /// Output EQ band edited but not saved yet (saved when the slider is released).
  pub out_eq_unsaved: Option<OutputLayer>,
  pub headphones_ui: super::tab_headphones::HeadphonesUi,
  pub sidetone_ui: SidetoneUi,
  /// Last output APO heartbeat value and when it changed (live indicator).
  pub out_apo_seen: (u32, Option<std::time::Instant>),

  /// Microphone voice profile (chat/media profiles live in `output_eq`).
  pub mic_profile: ParametricProfile,
  /// The APO mapping, read-only here: how many app streams each channel has (opened lazily).
  apo_view: Option<SharedMemoryClient>,
  apo_view_tried: Option<std::time::Instant>,

  pub available_mic_profiles: Vec<super::mic_presets::MicPreset>,
  pub profile_bar: super::profile_bar::ProfileBarUi,

  // Shared Memory handle to VST / Daemon
  shm_handle: Option<HANDLE>,
  shm_ptr: Option<NonNull<ShmLayout>>,

  // Storage
  pub storage: ProfileStorage,

  // Mic tab: AEC plot history
  pub aec_history: VecDeque<AecHistoryPoint>,
  /// Mic EQ changed but not yet written to disk (saved when the slider is released).
  pub mic_eq_unsaved: bool,
  pub apo_banner: super::apo_banner::ApoBanner,
  pub uninstall_ui: super::tab_settings::UninstallUi,
  pub transfer_ui: super::tab_settings::TransferUi,
  pub setup: super::setup::SetupUi,
  pub log_view: super::tab_log::LogView,
  aec_last_seq: u32,
  /// Last mic DSP telemetry seq and when it changed (is any mic stream processing?).
  /// None until first read, so a seq left over in shared memory doesn't count as live.
  mic_dsp_seen: (Option<u32>, Option<std::time::Instant>),
  denoise_switched_at: Option<std::time::Instant>,
  /// Mic chain + mixer choices as last written to disk (see `save_audio_settings_if_changed`).
  audio_settings_saved: super::audio_settings::AudioSettings,
  /// Double-talk lasts ~30 ms: keep the indicator lit long enough to read.
  double_talk_until: Option<std::time::Instant>,
  started_at: std::time::Instant,
}

impl AppState {
  pub fn init(storage: ProfileStorage) -> Self {
    let available_mic_profiles = super::mic_presets::all_mic_presets(&storage);
    let mic_profile = available_mic_profiles
      .iter()
      .find(|p| p.profile.name == super::mic_presets::DEFAULT_PRESET)
      .map(|p| p.profile.clone())
      .unwrap_or_else(|| ParametricProfile::new(super::mic_presets::DEFAULT_PRESET, VadType::Mic));

    let (shm_handle, shm_ptr) = open_or_create_shm(storage.base_dir());
    // SAFETY: as in `shm_header`; the mapping is unmapped only in Drop.
    let shm = shm_ptr.map(|p| unsafe { &(*p.as_ptr()).header });
    let audio_settings_saved = shm.map(super::audio_settings::AudioSettings::from_header).unwrap_or_default();
    let selected_mic_endpoint_id = shm.and_then(|h| h.get_selected_mic_id());

    let mic_profile = load_mic_profile(&storage).unwrap_or(mic_profile);
    crate::output_eq::game_presets::seed_builtin_game_presets(&storage);
    // Before the game EQ loads its profiles (window and service): presets a GG update added.
    crate::sonar_importer::sync_new_sonar_profiles(&storage);
    let output_eq = OutputEq::new(storage.clone());
    let autostart_enabled = crate::autostart::is_autostart_enabled();
    let prefs = super::prefs::UiPrefs::load_or_default(storage.base_dir());
    crate::i18n::set_language(prefs.language);

    let state = Self {
      active_tab: ActiveTab::Mixer,
      endpoints: Vec::new(),
      capture_endpoints: Vec::new(),
      selected_endpoint_id: None,
      selected_chat_endpoint_id: None,
      selected_mic_endpoint_id,
      spatial_status: SpatialAudioStatus::Disabled,
      spatial_format: None,
      spatial_expected: None,
      spatial_output_id: None,
      spatial_checked_at: None,
      active_game_name: None,
      autostart_enabled,
      prefs,
      output_eq,
      out_eq_unsaved: None,
      headphones_ui: Default::default(),
      sidetone_ui: Default::default(),
      out_apo_seen: (0, None),
      mic_profile,
      apo_view: None,
      apo_view_tried: None,
      available_mic_profiles,
      profile_bar: Default::default(),
      shm_handle,
      shm_ptr,
      storage,
      aec_history: VecDeque::with_capacity(AEC_HISTORY_LEN),
      mic_eq_unsaved: false,
      apo_banner: Default::default(),
      uninstall_ui: Default::default(),
      transfer_ui: Default::default(),
      setup: Default::default(),
      log_view: Default::default(),
      aec_last_seq: 0,
      mic_dsp_seen: (None, None),
      denoise_switched_at: None,
      audio_settings_saved,
      double_talk_until: None,
      started_at: std::time::Instant::now(),
    };
    state.sync_mic_eq_to_shm();
    state
  }

  /// Run `f` on the output EQ engine with the shared-memory header (both borrowed at once).
  pub fn output_eq_do<R>(&mut self, f: impl FnOnce(&mut OutputEq, Option<&ShmHeader>) -> R) -> R {
    // SAFETY: the mapping lives as long as self (unmapped in Drop); the header is only
    // accessed through atomics, so sharing it with the engine borrow is sound.
    let shm = self.shm_ptr.map(|p| unsafe { &(*p.as_ptr()).header });
    f(&mut self.output_eq, shm)
  }

  /// App streams the stream EQ currently runs on `bus` (paired by the daemon); None while
  /// no stream APO has created its mapping (not installed / nothing played since boot).
  pub fn streams_on_bus(&mut self, bus: StreamBus) -> Option<usize> {
    if self.apo_view.is_none() && self.apo_view_tried.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(2)) {
      self.apo_view_tried = Some(std::time::Instant::now());
      self.apo_view = SharedMemoryClient::open_existing(APO_SHM_NAME).ok();
    }
    let table = &self.apo_view.as_ref()?.layout().streams;
    let wanted = bus.to_u32();
    Some(
      table
        .slots
        .iter()
        .filter(|s| s.owner.load(Ordering::Relaxed) != 0 && s.bus.load(Ordering::Relaxed) == wanted)
        .count(),
    )
  }

  /// Publish the output EQ (headphone + game) and the master switch.
  pub fn apply_output_eq(&mut self) {
    self.output_eq_do(|eq, shm| eq.apply(shm));
  }

  /// Output APO processed audio within the last 2 s.
  pub fn output_apo_live(&mut self) -> bool {
    let blocks = self.shm_header().map_or(0, |h| h.out_apo_blocks.load(Ordering::Relaxed));
    let now = std::time::Instant::now();
    if blocks != self.out_apo_seen.0 {
      self.out_apo_seen = (blocks, Some(now));
    }
    self.out_apo_seen.1.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_secs(2))
  }

  pub fn shm_header(&self) -> Option<&ShmHeader> {
    // SAFETY: the view lives as long as `self` (unmapped in `Drop`); other processes change the header only through
    // atomics after setup.
    self.shm_ptr.map(|ptr| unsafe { &ptr.as_ref().header })
  }

  pub fn shm_header_mut(&mut self) -> Option<&mut ShmHeader> {
    // SAFETY: as `shm_header`; `&mut self` keeps this process's references to the header exclusive.
    self.shm_ptr.map(|mut ptr| unsafe { &mut ptr.as_mut().header })
  }

  pub fn set_shm_selected_mic(&mut self, id: &str) {
    if let Some(h) = self.shm_header_mut() {
      h.set_selected_mic_id(id);
    }
  }

  pub fn is_daemon_active(&self) -> bool {
    self.shm_header().map(|h| h.daemon_active.load(Ordering::Relaxed)).unwrap_or(false)
  }

  pub fn signal_daemon_shutdown(&self) {
    if let Some(h) = self.shm_header() {
      h.shutdown_daemon_signal.store(true, Ordering::SeqCst);
    }
  }

  pub fn sync_mixer_to_shm(&self, chatmix: &ChatMixController) {
    if let Some(h) = self.shm_header() {
      h.master_volume_permille.store((chatmix.master_volume * 1000.0).round() as u32, Ordering::Relaxed);
      h.game_volume_permille.store((chatmix.game_volume * 1000.0).round() as u32, Ordering::Relaxed);
      h.chat_volume_permille.store((chatmix.chat_volume * 1000.0).round() as u32, Ordering::Relaxed);
      h.media_volume_permille.store((chatmix.media_volume * 1000.0).round() as u32, Ordering::Relaxed);
      h.mic_volume_permille.store((chatmix.mic_volume * 1000.0).round() as u32, Ordering::Relaxed);

      h.master_muted.store(chatmix.master_muted, Ordering::Relaxed);
      h.game_muted.store(chatmix.game_muted, Ordering::Relaxed);
      h.chat_muted.store(chatmix.chat_muted, Ordering::Relaxed);
      h.media_muted.store(chatmix.media_muted, Ordering::Relaxed);
      h.mic_muted.store(chatmix.mic_muted, Ordering::Relaxed);

      h.chatmix_balance_permille.store((chatmix.balance * 1000.0).round() as i32, Ordering::Relaxed);
    }
  }

  pub fn update_shm_mic_controls(&self, aec_enabled: bool, ai_strength: f32, gate_db: i32) {
    if let Some(header) = self.shm_header() {
      header.aec_enabled.store(aec_enabled, Ordering::Relaxed);
      let permille = (ai_strength.clamp(0.0, 1.0) * 1000.0).round() as u32;
      header.ai_voice_strength_permille.store(permille, Ordering::Relaxed);
      header.noise_gate_threshold_db.store(gate_db, Ordering::Relaxed);
    }
  }

  /// Push the mic EQ to the DSP (live) without touching the file.
  pub fn sync_mic_eq_to_shm(&self) {
    if let Some(h) = self.shm_header() {
      h.write_mic_eq(&self.mic_profile.bands);
    }
  }

  /// Push the mic EQ to the DSP and remember it across restarts.
  pub fn apply_mic_eq(&mut self) {
    self.mic_profile.bands.truncate(MAX_EQ_BANDS);
    self.sync_mic_eq_to_shm();
    save_mic_profile(&self.storage, &self.mic_profile);
  }

  /// Make `profile` the mic EQ (no unsaved edits left).
  pub fn use_mic_profile(&mut self, profile: ParametricProfile) {
    self.mic_profile = profile;
    self.mic_eq_unsaved = false;
    self.apply_mic_eq();
  }

  /// Save `profile` as one of the user's mic presets and make it the mic EQ.
  pub fn save_mic_preset(&mut self, profile: ParametricProfile) -> std::io::Result<()> {
    super::mic_presets::save_user_preset(&self.storage, &profile)?;
    self.use_mic_profile(profile);
    self.available_mic_profiles = super::mic_presets::all_mic_presets(&self.storage);
    Ok(())
  }

  pub fn delete_mic_preset(&mut self, name: &str) -> std::io::Result<()> {
    super::mic_presets::delete_user_preset(&self.storage, name)?;
    self.available_mic_profiles = super::mic_presets::all_mic_presets(&self.storage);
    Ok(())
  }

  /// Remember the mic chain and mixer across restarts. Call once per frame; writes only when
  /// something changed and no slider is being dragged.
  pub fn save_audio_settings_if_changed(&mut self, dragging: bool) {
    if dragging {
      return;
    }
    let Some(now) = self.shm_header().map(super::audio_settings::AudioSettings::from_header) else {
      return;
    };
    if now != self.audio_settings_saved {
      now.save(self.storage.base_dir());
      self.audio_settings_saved = now;
    }
  }

  pub fn denoise_models(&self) -> (DenoiseModel, DenoiseModel) {
    self.shm_header().map_or((DenoiseModel::RnNoise, DenoiseModel::RnNoise), |h| {
      (
        DenoiseModel::from_u32(h.denoise_model.load(Ordering::Relaxed)),
        DenoiseModel::from_u32(h.denoise_model_active.load(Ordering::Relaxed)),
      )
    })
  }

  pub fn set_denoise_model(&mut self, model: DenoiseModel) {
    if let Some(h) = self.shm_header() {
      h.denoise_model.store(model.to_u32(), Ordering::Relaxed);
    }
    self.denoise_switched_at = Some(std::time::Instant::now());
  }

  /// The selected model picked again: the mic DSP tries it once more (failed load, overload).
  pub fn retry_denoise_model(&mut self) {
    if let Some(h) = self.shm_header() {
      h.denoise_retry_seq.fetch_add(1, Ordering::Relaxed);
    }
    self.denoise_switched_at = Some(std::time::Instant::now());
  }

  /// A mic DSP (APO or bridge) published telemetry within the last 2 s. Call once per frame
  /// or more; repeated calls in one frame see the same seq and change nothing.
  pub fn mic_dsp_live(&mut self) -> bool {
    let seq = self.shm_header().map_or(0, |h| h.aec_telemetry_seq.load(Ordering::Acquire));
    let now = std::time::Instant::now();
    match self.mic_dsp_seen.0 {
      None => self.mic_dsp_seen.0 = Some(seq),
      Some(seen) if seen != seq => {
        // First change after being idle = the stream just (re)started and loads its model.
        if self.mic_dsp_seen.1.is_none_or(|t| now.duration_since(t) >= MIC_DSP_LIVE_WINDOW) {
          self.denoise_switched_at = Some(now);
        }
        self.mic_dsp_seen = (Some(seq), Some(now));
      }
      Some(_) => {}
    }
    self.mic_dsp_seen.1.is_some_and(|t| now.duration_since(t) < MIC_DSP_LIVE_WINDOW)
  }

  /// What the noise suppression is really doing. `active` in shared memory is only updated
  /// while a mic DSP (APO or bridge) processes audio, so without one it is stale.
  pub fn denoise_status(&mut self) -> DenoiseStatus {
    let (selected, active) = self.denoise_models();
    let live = self.mic_dsp_live();
    let now = std::time::Instant::now();
    let settling = self.denoise_switched_at.is_some_and(|t| now.duration_since(t) < DENOISE_SWITCH_GRACE);
    let limit =
      self.shm_header().map_or(DenoiseLimit::None, |h| DenoiseLimit::from_u32(h.denoise_limit.load(Ordering::Relaxed)));
    if live && limit != DenoiseLimit::None {
      return DenoiseStatus::Limited { limit, running: active };
    }
    match (live, selected == active) {
      (false, _) => DenoiseStatus::Idle(selected),
      (true, true) => DenoiseStatus::Running(active),
      (true, false) if settling => DenoiseStatus::Switching(active),
      // The audio thread loads the model synchronously; still the old one after the grace period = load failed.
      (true, false) => DenoiseStatus::Failed { selected, running: active },
    }
  }

  /// Delay the mic chain adds: measured end to end in the DSP, and what each stage holds now.
  pub fn mic_latency(&self) -> MicLatency {
    self.shm_header().map_or(MicLatency::default(), |h| {
      let ms = |us: u32| us as f32 / 1000.0;
      let measured = h.mic_latency_measured_us.load(Ordering::Relaxed);
      MicLatency {
        measured_ms: (measured > 0).then(|| ms(measured)),
        aec_ms: ms(h.mic_latency_aec_us.load(Ordering::Relaxed)),
        denoise_ms: ms(h.mic_latency_denoise_us.load(Ordering::Relaxed)),
        lookahead_ms: ms(h.mic_latency_lookahead_us.load(Ordering::Relaxed)),
      }
    })
  }

  pub fn mic_dsp_load(&self) -> DspLoad {
    self.shm_header().map_or(DspLoad::default(), |h| DspLoad {
      avg: h.mic_dsp_load_avg_permille.load(Ordering::Relaxed) as f32 / 1000.0,
      peak: h.mic_dsp_load_peak_permille.load(Ordering::Relaxed) as f32 / 1000.0,
      late_blocks: h.mic_dsp_overruns.load(Ordering::Relaxed),
    })
  }

  /// Read AEC telemetry and append to the plot history when the DSP published new data.
  pub fn poll_aec_telemetry(&mut self) -> AecTelemetry {
    let Some(h) = self.shm_header() else {
      return AecTelemetry::default();
    };
    let us_to_ms = |us: u32| us as f32 / 1000.0;
    let delay_us = h.aec_delay_us.load(Ordering::Relaxed);
    let raw_us = h.aec_raw_delay_us.load(Ordering::Relaxed);
    let mut t = AecTelemetry {
      state: Some(AecState::from_u32(h.aec_state.load(Ordering::Relaxed))),
      delay_ms: (delay_us > 0).then(|| us_to_ms(delay_us)),
      raw_delay_ms: (raw_us > 0).then(|| us_to_ms(raw_us)),
      from_cache: h.aec_delay_from_cache.load(Ordering::Relaxed),
      confidence: h.aec_confidence_permille.load(Ordering::Relaxed) as f32 / 1000.0,
      estimate_count: h.aec_estimate_count.load(Ordering::Relaxed),
      erle_db: h.erle_db_times_10.load(Ordering::Relaxed) as f32 / 10.0,
      double_talk: h.aec_double_talk.load(Ordering::Relaxed),
      ref_db: h.aec_ref_level_db_x10.load(Ordering::Relaxed) as f32 / 10.0,
      mic_db: h.aec_mic_level_db_x10.load(Ordering::Relaxed) as f32 / 10.0,
      out_db: h.aec_out_level_db_x10.load(Ordering::Relaxed) as f32 / 10.0,
      filter_taps: h.aec_filter_taps.load(Ordering::Relaxed),
      pre_delay_ms: us_to_ms(h.aec_pre_delay_us.load(Ordering::Relaxed)),
      corr_max_delay_ms: us_to_ms(h.aec_corr_max_delay_us.load(Ordering::Relaxed)),
      impulse: h.read_aec_impulse(),
      corr: h.read_aec_corr(),
      seq: h.aec_telemetry_seq.load(Ordering::Acquire),
      live: false,
    };
    t.live = self.mic_dsp_live();
    let now = std::time::Instant::now();
    if t.double_talk {
      self.double_talk_until = Some(now + DOUBLE_TALK_HOLD);
    }
    t.double_talk = self.double_talk_until.is_some_and(|until| until > now);
    if t.seq != self.aec_last_seq {
      self.aec_last_seq = t.seq;
      if self.aec_history.len() >= AEC_HISTORY_LEN {
        self.aec_history.pop_front();
      }
      self.aec_history.push_back(AecHistoryPoint {
        t: self.started_at.elapsed().as_secs_f64(),
        erle_db: t.erle_db,
        delay_ms: t.delay_ms,
      });
    }
    t
  }
}

impl Drop for AppState {
  fn drop(&mut self) {
    if let Some(ptr) = self.shm_ptr {
      // SAFETY: the view from `open_or_create_shm`, unmapped once; the references `shm_header` hands out borrow
      // `self`, so none outlives this.
      unsafe {
        let _ =
          UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: ptr.as_ptr() as *mut _ });
      }
    }
    if let Some(h) = self.shm_handle {
      // SAFETY: the mapping handle from `open_or_create_shm`, closed once.
      unsafe {
        let _ = CloseHandle(h);
      }
    }
  }
}

fn mic_profile_path(storage: &ProfileStorage) -> PathBuf {
  storage.base_dir().join(MIC_EQ_FILE)
}

/// Missing/corrupt file -> None (defaults are used), logged.
fn load_mic_profile(storage: &ProfileStorage) -> Option<ParametricProfile> {
  let path = mic_profile_path(storage);
  let text = std::fs::read_to_string(&path).ok()?;
  match serde_json::from_str::<ParametricProfile>(&text) {
    Ok(mut p) => {
      // Saved before cuts had a steepness: two stacked rows become one.
      p.bands = collapse_slopes(&p.bands);
      p.bands.iter_mut().for_each(|b| b.clamp_parameters());
      while slots_used(&p.bands) > MAX_EQ_BANDS {
        p.bands.pop();
      }
      Some(p)
    }
    Err(e) => {
      log::warn!("[MicEQ] {} is corrupt ({}), using defaults", path.display(), e);
      crate::output_eq::settings::keep_corrupt(&path);
      None
    }
  }
}

fn save_mic_profile(storage: &ProfileStorage, profile: &ParametricProfile) {
  let path = mic_profile_path(storage);
  let result = (|| -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
      std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(profile).map_err(std::io::Error::other)?;
    crate::output_eq::settings::write_atomic(&path, &json)
  })();
  if let Err(e) = result {
    log::warn!("[MicEQ] could not save {}: {}", path.display(), e);
  }
}

/// Open the daemon <-> window mapping; a new one (nothing else runs) gets the defaults, the
/// Windows volumes and then the user's saved choices from `settings_dir`.
pub(crate) fn open_or_create_shm(settings_dir: &Path) -> (Option<HANDLE>, Option<NonNull<ShmLayout>>) {
  // SAFETY: the name is NUL-terminated and outlives the calls; the view is the layout's size or null (checked, the
  // handle is then closed). Defaults are written only into a mapping no one set up yet (magic not set), before the
  // pointer is handed out.
  unsafe {
    let wide_name: Vec<u16> = SHM_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let size_bytes = ShmLayout::total_size_bytes() as u32;

    let handle =
      match CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, size_bytes, PCWSTR(wide_name.as_ptr())) {
        Ok(h) => h,
        Err(_) => return (None, None),
      };

    let map_ptr = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size_bytes as usize);
    if map_ptr.Value.is_null() {
      let _ = CloseHandle(handle);
      return (None, None);
    }

    let ptr = NonNull::new(map_ptr.Value as *mut ShmLayout);
    if let Some(p) = ptr {
      let header = &mut (*p.as_ptr()).header;
      if header.magic != gamersequalizer_common::shm::SHM_MAGIC {
        header.init_defaults();
        if let Some((vol, mute)) = crate::win_audio::chatmix::get_system_master_volume() {
          header.master_volume_permille.store((vol * 1000.0).round() as u32, Ordering::Relaxed);
          header.master_muted.store(mute, Ordering::Relaxed);
        }
        if let Some((vol, mute)) = crate::win_audio::chatmix::get_system_mic_volume() {
          header.mic_volume_permille.store((vol * 1000.0).round() as u32, Ordering::Relaxed);
          header.mic_muted.store(mute, Ordering::Relaxed);
        }
        super::audio_settings::AudioSettings::load_or_default(settings_dir).apply_to_header(header);
      }
    }
    (Some(handle), ptr)
  }
}
