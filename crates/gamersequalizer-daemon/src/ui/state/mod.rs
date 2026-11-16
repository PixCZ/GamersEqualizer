pub use crate::output_eq::{OutputEq, OutputLayer};
use crate::shm_mapping::open_or_create_shm;
use crate::sonar_importer::ProfileStorage;
use crate::win_audio::{AudioEndpointInfo, ChatMixController, SpatialAudioStatus, SpatialFormat};
use gamersequalizer_common::biquad::MAX_EQ_BANDS;
use gamersequalizer_common::profile::{ParametricProfile, VadType, collapse_slopes, slots_used};
use gamersequalizer_common::shm::{APO_SHM_NAME, AecState, DenoiseLimit, DenoiseModel, ShmHeader, ShmLayout};
use gamersequalizer_common::sidetone::SidetonePair;
use gamersequalizer_common::streams::StreamBus;
use gamersequalizer_mic_chain::shared_memory::SharedMemoryClient;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::UnmapViewOfFile;

mod mic_presets;
mod mic_status;

use mic_presets::*;
pub use mic_status::*;

/// AEC history kept for the plots (~60 s at 10 updates/s).
const AEC_HISTORY_LEN: usize = 600;

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
