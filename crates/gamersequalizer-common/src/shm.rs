use crate::biquad::{MAX_EQ_BANDS, MAX_OUT_EQ_BANDS, MAX_STREAM_EQ_BANDS};
use crate::profile::{expand_slopes, BiquadBand, FilterType};
use crate::streams::{StreamTable, STREAM_EQ_BUSES};
use std::sync::atomic::{fence, AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Name carries the layout version: a process still running an older build keeps its own
/// (smaller) mapping instead of both reading each other's memory with different layouts.
/// Bump both names together with `SHM_VERSION` on every layout change.
pub const SHM_NAME: &str = "Local\\GamersEqualizer_SpeakerRef_Buffer_v13";
/// Mapping created by the capture APO inside audiodg (session 0, so it must be Global; only
/// a service may create Global objects). The daemon opens it when it exists and mirrors
/// reference audio + settings in, telemetry out. Same layout as `SHM_NAME`.
pub const APO_SHM_NAME: &str = "Global\\GamersEqualizer_Apo_v13";
pub const SHM_MAGIC: u32 = 0x474D4551; // 'GMEQ'
pub const SHM_VERSION: u32 = 13;
/// Voice leveler target (`mic_leveler_target_db_x10`) until the user changes it: -24 dBFS.
pub const MIC_LEVELER_DEFAULT_TARGET_DB_X10: i32 = -240;
/// 131072 frames = ~2.7 s at 48 kHz: covers the delay estimator's 1 s window + 400 ms search.
pub const RING_BUFFER_CAPACITY: usize = 131072;
/// Reference samples read from the ring are clamped to +-this (+18 dBFS; a float mix may go
/// above 1.0). The ring is written by another process: one inf/NaN/huge sample would otherwise
/// turn the AEC output into NaN and pin the speaker level meter at +inf forever.
const REF_SAMPLE_MAX: f32 = 8.0;
pub const REF_SAMPLE_RATE: u32 = 48_000;
pub const AEC_IMPULSE_POINTS: usize = 64;
pub const AEC_CORR_POINTS: usize = 128;
const HNS_PER_SEC: i64 = 10_000_000;
/// UTF-16 slots of the active-game strings (profile name, label).
pub const GAME_TEXT_LEN: usize = 128;
/// `noise_gate_threshold_db` values. The gate is automatic (keyed by a voice detector), so the
/// field is only a mode now: at or below `NOISE_GATE_OFF` = off, `NOISE_GATE_AUTO_LOOKAHEAD`
/// (or above) = automatic with word-onset protection (the DSP delays the output so the gate
/// opens early), anything between = automatic. Old DSP builds read the lookahead value as "auto".
pub const NOISE_GATE_AUTO: i32 = 0;
pub const NOISE_GATE_AUTO_LOOKAHEAD: i32 = 1;
pub const NOISE_GATE_OFF: i32 = -80;
/// The APO processes only while `owner_beat` changed within this time (the owner bumps it ~10x/s).
pub const OWNER_TIMEOUT: Duration = Duration::from_secs(3);

/// One parametric EQ band in shared memory (f32 values stored as bits).
#[repr(C)]
#[derive(Default)]
pub struct ShmEqBand {
  pub enabled: AtomicU32,
  pub filter_type: AtomicU32,
  pub frequency_bits: AtomicU32,
  pub gain_bits: AtomicU32,
  pub q_bits: AtomicU32,
}

/// EQ the stream APO runs on one channel's app streams (chat or media), before the mix.
#[repr(C)]
pub struct ShmStreamEq {
  pub version: AtomicU32,
  pub band_count: AtomicU32,
  pub preamp_db_bits: AtomicU32,
  pub bands: [ShmEqBand; MAX_STREAM_EQ_BANDS],
}

/// Source EQ versions a mirror (`ShmHeader::copy_controls_from`) copied last. Lives in the
/// copying process, not in shared memory; `default()` = nothing copied yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopiedEqVersions {
  mic: Option<u32>,
  out: Option<u32>,
  streams: [Option<u32>; STREAM_EQ_BUSES],
}

/// What the echo canceller is doing right now (shown in the UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AecState {
  Disabled,
  /// No loopback reference is being captured at all.
  NoReference,
  /// Reference stream exists but nothing is playing.
  ReferenceSilent,
  /// Playing, delay not yet confirmed.
  Measuring,
  /// Delay confirmed, filter running.
  Locked,
  Unknown(u32),
}

/// Noise suppression model chosen in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenoiseModel {
  RnNoise,
  DeepFilterNet,
  /// DPDFNet-2 48 kHz (Ceva, DeepFilterNet2 + dual-path RNN): best on soft speech, ~3x the CPU.
  DpdfNet,
  Unknown(u32),
}

/// Why the selected noise model is not the one running (DSP -> GUI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenoiseLimit {
  None,
  /// The selected heavy model (DeepFilterNet / DPDFNet) could not be loaded; RNNoise runs.
  LoadFailed,
  /// The selected heavy model missed the real-time deadline repeatedly; switched to RNNoise
  /// until the model is selected again or the stream restarts.
  Overloaded,
  Unknown(u32),
}

/// Lock-free header inside the Windows shared memory mapping
#[repr(C)]
pub struct ShmHeader {
  pub magic: u32,
  pub version: u32,
  pub sample_rate: u32,
  pub channels: u32,
  pub capacity_frames: u32,

  // Single-producer single-consumer ring buffer head/tail
  pub write_frame_index: AtomicU32,
  pub read_frame_index: AtomicU32,

  // Runtime control flags updated by GUI / Daemon
  pub aec_enabled: AtomicBool,
  pub ai_voice_strength_permille: AtomicU32, // 0 to 1000 (0.0 to 1.0)
  pub noise_gate_threshold_db: AtomicI32,    // NOISE_GATE_AUTO[_LOOKAHEAD] or NOISE_GATE_OFF

  // Telemetry fed back by VST to GUI
  pub erle_db_times_10: AtomicI32, // e.g. 185 = 18.5 dB echo return loss enhancement
  pub tap_active: AtomicBool,
  pub mic_active: AtomicBool,

  // Multi-process IPC between Background Daemon and Standalone UI Tool
  pub daemon_active: AtomicBool,
  pub shutdown_daemon_signal: AtomicBool,
  pub master_volume_permille: AtomicU32,
  pub game_volume_permille: AtomicU32,
  pub chat_volume_permille: AtomicU32,
  pub media_volume_permille: AtomicU32,
  pub mic_volume_permille: AtomicU32,
  pub master_muted: AtomicBool,
  pub game_muted: AtomicBool,
  pub chat_muted: AtomicBool,
  pub media_muted: AtomicBool,
  pub mic_muted: AtomicBool,
  pub chatmix_balance_permille: AtomicI32,
  pub selected_mic_id: [u16; 64],
  pub mic_id_version: AtomicU32,

  // --- v3: microphone EQ (GUI -> DSP) ---
  pub mic_eq_version: AtomicU32,
  pub mic_eq_band_count: AtomicU32,
  pub mic_eq_bands: [ShmEqBand; MAX_EQ_BANDS],

  // --- v3: noise suppression model (GUI -> DSP, DSP reports what actually runs) ---
  pub denoise_model: AtomicU32,
  pub denoise_model_active: AtomicU32,

  // --- v3: reference timing (loopback writer -> DSP), guarded by a seqlock ---
  /// Odd while the writer updates the snapshot.
  pub ref_seq: AtomicU32,
  /// Ring index whose frame plays at `ref_snap_qpc_hns`.
  pub ref_snap_index: AtomicU32,
  /// QPC time in 100 ns units; 0 = no reference captured yet.
  pub ref_snap_qpc_hns: AtomicU64,

  // --- v3: AEC telemetry (DSP -> GUI) ---
  pub aec_state: AtomicU32,
  pub aec_delay_us: AtomicU32,
  pub aec_raw_delay_us: AtomicU32,
  pub aec_confidence_permille: AtomicU32,
  pub aec_estimate_count: AtomicU32,
  pub aec_delay_from_cache: AtomicBool,
  pub aec_double_talk: AtomicBool,
  pub aec_ref_level_db_x10: AtomicI32,
  pub aec_mic_level_db_x10: AtomicI32,
  pub aec_out_level_db_x10: AtomicI32,
  pub aec_filter_taps: AtomicU32,
  pub aec_pre_delay_us: AtomicU32,
  pub aec_telemetry_seq: AtomicU32,
  /// Adaptive filter impulse response magnitude (f32 bits), tap groups oldest..newest.
  pub aec_impulse: [AtomicU32; AEC_IMPULSE_POINTS],
  /// Cross-correlation magnitude over 0..max delay (f32 bits).
  pub aec_corr: [AtomicU32; AEC_CORR_POINTS],
  pub aec_corr_max_delay_us: AtomicU32,

  // --- v4: echo delay remembered by the daemon (daemon -> DSP), 48 kHz samples, 0 = none.
  // The APO (LOCAL SERVICE) can't read the user's cache file, so it gets the value here.
  pub aec_cached_delay_samples: AtomicU32,
  // --- v12: the reference output changed (daemon -> DSP). Bumped *after* the new output's
  // `aec_cached_delay_samples` is stored: the DSP drops its delay lock and starts from that.
  pub aec_reference_seq: AtomicU32,
  /// DSP -> daemon: the `aec_reference_seq` the reported delay belongs to. The daemon saves a
  /// delay only when this matches, so the old output's delay never lands under the new key.
  pub aec_reference_seq_seen: AtomicU32,

  // --- v5: master switch + output EQ (headphone correction + game profile) ---
  /// Everything off: the output EQ and the mic chain pass audio through unchanged.
  pub master_bypass: AtomicBool,
  /// Combined output EQ (only the enabled layers, headphone first, then game).
  pub out_eq_version: AtomicU32,
  pub out_eq_band_count: AtomicU32,
  pub out_preamp_db_bits: AtomicU32,
  pub out_eq_bands: [ShmEqBand; MAX_OUT_EQ_BANDS],
  /// Bumped after output EQ settings/profiles were saved: other processes reload them.
  pub out_settings_version: AtomicU32,
  /// Game whose profile is active, written by the process that runs game detection.
  /// Seqlock: odd while being written.
  pub active_game_seq: AtomicU32,
  pub active_game_profile: [AtomicU16; GAME_TEXT_LEN],
  pub active_game_label: [AtomicU16; GAME_TEXT_LEN],
  /// Output APO telemetry (APO -> UI): blocks processed (heartbeat), stream format.
  pub out_apo_blocks: AtomicU32,
  pub out_apo_sample_rate: AtomicU32,
  pub out_apo_channels: AtomicU32,

  // --- v6: mic DSP CPU load (DSP -> GUI) ---
  /// Processing time / block duration, last telemetry window: peak and average (1000 = 100 %).
  pub mic_dsp_load_peak_permille: AtomicU32,
  pub mic_dsp_load_avg_permille: AtomicU32,
  /// Blocks (cumulative) whose processing took longer than the block lasts = audible glitch risk.
  pub mic_dsp_overruns: AtomicU32,
  pub denoise_limit: AtomicU32,

  // --- v8: stream EQ per channel (daemon -> stream APO), `StreamBus::WITH_EQ` order ---
  pub stream_eq: [ShmStreamEq; STREAM_EQ_BUSES],

  // --- v9: owner heartbeat (daemon of the active console session -> APO) ---
  /// Bumped by the one daemon whose settings the APO follows. Unchanged for `OWNER_TIMEOUT`
  /// (nobody runs the app, it exited or crashed, another Windows user is at the console) =
  /// the APO passes all audio through untouched: it never applies stale or someone else's settings.
  pub owner_beat: AtomicU32,

  // --- v10: learning voice leveler ---
  /// GUI -> DSP: bring speech to one level (after the voice EQ, before boost/limiter).
  pub mic_leveler_enabled: AtomicBool,
  /// DSP -> GUI: gain the leveler applies now, dB x 10.
  pub mic_leveler_gain_db_x10: AtomicI32,
  /// GUI -> DSP: output loudness the user wants, dBFS x 10.
  pub mic_leveler_target_db_x10: AtomicI32,
  /// GUI -> DSP: bumped by "Learn again" (forget the learned level).
  pub mic_leveler_reset_seq: AtomicU32,
  /// Daemon -> DSP: level learned earlier for this microphone, dBFS x 10 (0 = none).
  pub mic_leveler_saved_level_db_x10: AtomicI32,
  /// DSP -> daemon: level learned so far, dBFS x 10 (0 = none yet); the daemon saves it per mic.
  pub mic_leveler_level_db_x10: AtomicI32,

  // --- v11: mic volume around the leveler, automatic input gain ---
  /// Daemon -> DSP: gain of the Windows mic volume (applied before the APO), dB x 10. The leveler
  /// learns the level without it, so a volume change is compensated at once (0 = 0 dB / unknown).
  pub mic_input_gain_db_x10: AtomicI32,
  /// DSP -> daemon: raw input samples at full scale (cumulative): the converter clipped.
  pub mic_input_clips: AtomicU32,
  /// DSP -> daemon: speech the leveler learned from (cumulative ms).
  pub mic_speech_ms: AtomicU32,
  /// Daemon -> GUI: Windows mic volume the automatic input gain set, permille (0 = none). The
  /// window must not read it as the user's volume.
  pub mic_auto_gain_permille: AtomicU32,

  // --- v13: timing of the audio engine's calls into the mic APO (APO -> daemon debug log) ---
  /// Largest gap between the interval of two calls and the block length, last ~1 s, in us.
  pub mic_call_jitter_peak_us: AtomicU32,
  /// Calls (cumulative) that came over 1.5 blocks after the previous one: the stream stalled
  /// and apps got their data late.
  pub mic_call_gaps: AtomicU32,
}

/// Watches `owner_beat` from the APO side, on its own clock (no time shared across sessions).
#[derive(Debug, Default)]
pub struct OwnerWatch {
  /// Last beat value and when it last changed (None = not seen changing yet).
  seen: Option<(u32, Option<Instant>)>,
}

/// Game detected by the detection owner (daemon, or the GUI without a daemon).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveGame {
  /// Game profile name; empty = detected game without a profile.
  pub profile: String,
  /// What was detected ("Tom Clancy's The Division 2 (division2.exe)").
  pub label: String,
}

/// A stereo sample frame in the ring buffer
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct StereoFrame {
  pub left: f32,
  pub right: f32,
}

/// Full shared memory segment layout
#[repr(C)]
pub struct ShmLayout {
  pub header: ShmHeader,
  pub buffer: [StereoFrame; RING_BUFFER_CAPACITY],
  /// v7: per-stream table of the render SFX instances (only used in the APO mapping).
  /// All zero in a fresh mapping = all slots free.
  pub streams: StreamTable,
}

/// Safe helper for reading and writing frames to the ring buffer
pub struct ShmRingBuffer<'a> {
  layout: &'a mut ShmLayout,
}

impl AecState {
  pub fn to_u32(self) -> u32 {
    match self {
      AecState::Disabled => 0,
      AecState::NoReference => 1,
      AecState::ReferenceSilent => 2,
      AecState::Measuring => 3,
      AecState::Locked => 4,
      AecState::Unknown(v) => v,
    }
  }

  pub fn from_u32(v: u32) -> Self {
    match v {
      0 => AecState::Disabled,
      1 => AecState::NoReference,
      2 => AecState::ReferenceSilent,
      3 => AecState::Measuring,
      4 => AecState::Locked,
      other => AecState::Unknown(other),
    }
  }
}

impl DenoiseModel {
  pub fn to_u32(self) -> u32 {
    match self {
      DenoiseModel::RnNoise => 0,
      DenoiseModel::DeepFilterNet => 1,
      DenoiseModel::DpdfNet => 2,
      DenoiseModel::Unknown(v) => v,
    }
  }

  pub fn from_u32(v: u32) -> Self {
    match v {
      0 => DenoiseModel::RnNoise,
      1 => DenoiseModel::DeepFilterNet,
      2 => DenoiseModel::DpdfNet,
      other => DenoiseModel::Unknown(other),
    }
  }

  /// Neural models that fall back to RNNoise when they fail to load or overload the CPU.
  pub fn is_heavy(self) -> bool {
    matches!(self, DenoiseModel::DeepFilterNet | DenoiseModel::DpdfNet)
  }
}

impl DenoiseLimit {
  pub fn to_u32(self) -> u32 {
    match self {
      DenoiseLimit::None => 0,
      DenoiseLimit::LoadFailed => 1,
      DenoiseLimit::Overloaded => 2,
      DenoiseLimit::Unknown(v) => v,
    }
  }

  pub fn from_u32(v: u32) -> Self {
    match v {
      0 => DenoiseLimit::None,
      1 => DenoiseLimit::LoadFailed,
      2 => DenoiseLimit::Overloaded,
      other => DenoiseLimit::Unknown(other),
    }
  }
}

impl Default for ShmHeader {
  fn default() -> Self {
    let mut h = Self {
      magic: SHM_MAGIC,
      version: SHM_VERSION,
      sample_rate: REF_SAMPLE_RATE,
      channels: 2,
      capacity_frames: RING_BUFFER_CAPACITY as u32,
      write_frame_index: AtomicU32::new(0),
      read_frame_index: AtomicU32::new(0),
      aec_enabled: AtomicBool::new(true),
      ai_voice_strength_permille: AtomicU32::new(750), // 75% default
      noise_gate_threshold_db: AtomicI32::new(NOISE_GATE_AUTO_LOOKAHEAD),
      erle_db_times_10: AtomicI32::new(0),
      tap_active: AtomicBool::new(false),
      mic_active: AtomicBool::new(false),
      daemon_active: AtomicBool::new(false),
      shutdown_daemon_signal: AtomicBool::new(false),
      master_volume_permille: AtomicU32::new(1000),
      game_volume_permille: AtomicU32::new(1000),
      chat_volume_permille: AtomicU32::new(1000),
      media_volume_permille: AtomicU32::new(1000),
      mic_volume_permille: AtomicU32::new(1000),
      master_muted: AtomicBool::new(false),
      game_muted: AtomicBool::new(false),
      chat_muted: AtomicBool::new(false),
      media_muted: AtomicBool::new(false),
      mic_muted: AtomicBool::new(false),
      chatmix_balance_permille: AtomicI32::new(0),
      selected_mic_id: [0u16; 64],
      mic_id_version: AtomicU32::new(0),
      mic_eq_version: AtomicU32::new(0),
      mic_eq_band_count: AtomicU32::new(0),
      mic_eq_bands: std::array::from_fn(|_| ShmEqBand::default()),
      denoise_model: AtomicU32::new(0), // set in init_defaults
      denoise_model_active: AtomicU32::new(0),
      ref_seq: AtomicU32::new(0),
      ref_snap_index: AtomicU32::new(0),
      ref_snap_qpc_hns: AtomicU64::new(0),
      aec_state: AtomicU32::new(0),
      aec_delay_us: AtomicU32::new(0),
      aec_raw_delay_us: AtomicU32::new(0),
      aec_confidence_permille: AtomicU32::new(0),
      aec_estimate_count: AtomicU32::new(0),
      aec_delay_from_cache: AtomicBool::new(false),
      aec_double_talk: AtomicBool::new(false),
      aec_ref_level_db_x10: AtomicI32::new(-1200),
      aec_mic_level_db_x10: AtomicI32::new(-1200),
      aec_out_level_db_x10: AtomicI32::new(-1200),
      aec_filter_taps: AtomicU32::new(0),
      aec_pre_delay_us: AtomicU32::new(0),
      aec_telemetry_seq: AtomicU32::new(0),
      aec_impulse: std::array::from_fn(|_| AtomicU32::new(0)),
      aec_corr: std::array::from_fn(|_| AtomicU32::new(0)),
      aec_corr_max_delay_us: AtomicU32::new(0),
      aec_cached_delay_samples: AtomicU32::new(0),
      aec_reference_seq: AtomicU32::new(0),
      aec_reference_seq_seen: AtomicU32::new(0),
      master_bypass: AtomicBool::new(false),
      out_eq_version: AtomicU32::new(0),
      out_eq_band_count: AtomicU32::new(0),
      out_preamp_db_bits: AtomicU32::new(0),
      out_eq_bands: std::array::from_fn(|_| ShmEqBand::default()),
      out_settings_version: AtomicU32::new(0),
      active_game_seq: AtomicU32::new(0),
      active_game_profile: std::array::from_fn(|_| AtomicU16::new(0)),
      active_game_label: std::array::from_fn(|_| AtomicU16::new(0)),
      out_apo_blocks: AtomicU32::new(0),
      out_apo_sample_rate: AtomicU32::new(0),
      out_apo_channels: AtomicU32::new(0),
      mic_dsp_load_peak_permille: AtomicU32::new(0),
      mic_dsp_load_avg_permille: AtomicU32::new(0),
      mic_dsp_overruns: AtomicU32::new(0),
      denoise_limit: AtomicU32::new(0),
      stream_eq: std::array::from_fn(|_| ShmStreamEq::default()),
      owner_beat: AtomicU32::new(0),
      mic_leveler_enabled: AtomicBool::new(true),
      mic_leveler_gain_db_x10: AtomicI32::new(0),
      mic_leveler_target_db_x10: AtomicI32::new(MIC_LEVELER_DEFAULT_TARGET_DB_X10),
      mic_leveler_reset_seq: AtomicU32::new(0),
      mic_leveler_saved_level_db_x10: AtomicI32::new(0),
      mic_leveler_level_db_x10: AtomicI32::new(0),
      mic_input_gain_db_x10: AtomicI32::new(0),
      mic_input_clips: AtomicU32::new(0),
      mic_speech_ms: AtomicU32::new(0),
      mic_auto_gain_permille: AtomicU32::new(0),
      mic_call_jitter_peak_us: AtomicU32::new(0),
      mic_call_gaps: AtomicU32::new(0),
    };
    h.init_defaults();
    h
  }
}

impl ShmHeader {
  pub fn set_selected_mic_id(&mut self, id: &str) {
    let mut arr = [0u16; 64];
    for (slot, c) in arr.iter_mut().zip(id.encode_utf16().take(63)) {
      *slot = c;
    }
    self.selected_mic_id = arr;
    self.mic_id_version.fetch_add(1, Ordering::SeqCst);
  }

  pub fn get_selected_mic_id(&self) -> Option<String> {
    let len = self.selected_mic_id.iter().position(|&c| c == 0).unwrap_or(64);
    if len == 0 {
      None
    } else {
      self.selected_mic_id.get(..len).and_then(|s| String::from_utf16(s).ok())
    }
  }

  pub fn init_defaults(&mut self) {
    self.magic = SHM_MAGIC;
    self.version = SHM_VERSION;
    self.sample_rate = REF_SAMPLE_RATE;
    self.channels = 2;
    self.capacity_frames = RING_BUFFER_CAPACITY as u32;
    self.aec_enabled.store(true, Ordering::Relaxed);
    self.ai_voice_strength_permille.store(750, Ordering::Relaxed);
    self.noise_gate_threshold_db.store(NOISE_GATE_AUTO_LOOKAHEAD, Ordering::Relaxed);
    self.master_volume_permille.store(1000, Ordering::Relaxed);
    self.game_volume_permille.store(1000, Ordering::Relaxed);
    self.chat_volume_permille.store(1000, Ordering::Relaxed);
    self.media_volume_permille.store(1000, Ordering::Relaxed);
    self.mic_volume_permille.store(1000, Ordering::Relaxed);
    self.master_muted.store(false, Ordering::Relaxed);
    self.game_muted.store(false, Ordering::Relaxed);
    self.chat_muted.store(false, Ordering::Relaxed);
    self.media_muted.store(false, Ordering::Relaxed);
    self.mic_muted.store(false, Ordering::Relaxed);
    self.chatmix_balance_permille.store(0, Ordering::Relaxed);
    // Best quality at ~1 % CPU; the DSP falls back to RNNoise if it fails to load or overloads.
    self.denoise_model.store(DenoiseModel::DeepFilterNet.to_u32(), Ordering::Relaxed);
    self.mic_leveler_enabled.store(true, Ordering::Relaxed);
    self.mic_leveler_target_db_x10.store(MIC_LEVELER_DEFAULT_TARGET_DB_X10, Ordering::Relaxed);
  }

  /// GUI -> DSP settings, for mirroring the daemon's mapping into the APO's. An EQ is copied
  /// when its source version differs from the one `seen` remembers (a new owner starts from
  /// `CopiedEqVersions::default()`: copies everything). The destination keeps its own counters
  /// (each write bumps them), so the APO always sees a change: storing the source's number
  /// could land on exactly the version the APO had cached from the previous owner.
  pub fn copy_controls_from(&self, src: &ShmHeader, seen: &mut CopiedEqVersions) {
    let r = Ordering::Relaxed;
    self.aec_enabled.store(src.aec_enabled.load(r), r);
    self.ai_voice_strength_permille.store(src.ai_voice_strength_permille.load(r), r);
    self.noise_gate_threshold_db.store(src.noise_gate_threshold_db.load(r), r);
    self.mic_volume_permille.store(src.mic_volume_permille.load(r), r);
    self.mic_muted.store(src.mic_muted.load(r), r);
    self.denoise_model.store(src.denoise_model.load(r), r);
    self.master_bypass.store(src.master_bypass.load(r), r);
    self.mic_leveler_enabled.store(src.mic_leveler_enabled.load(r), r);
    self.mic_leveler_target_db_x10.store(src.mic_leveler_target_db_x10.load(r), r);
    self.mic_leveler_reset_seq.store(src.mic_leveler_reset_seq.load(r), r);
    self.mic_input_gain_db_x10.store(src.mic_input_gain_db_x10.load(r), r);
    // Version first, bands after: a write in between is copied now and again next time.
    let version = src.mic_eq_version.load(Ordering::Acquire);
    if seen.mic != Some(version) {
      self.write_mic_eq(&src.read_mic_eq());
      seen.mic = Some(version);
    }
    let version = src.out_eq_version.load(Ordering::Acquire);
    if seen.out != Some(version) {
      let (bands, preamp) = src.read_out_eq();
      self.write_out_eq(&bands, preamp);
      seen.out = Some(version);
    }
    for ((dst, src), seen) in self.stream_eq.iter().zip(&src.stream_eq).zip(&mut seen.streams) {
      let version = src.version.load(Ordering::Acquire);
      if *seen != Some(version) {
        let (bands, preamp) = src.read();
        dst.write(&bands, preamp);
        *seen = Some(version);
      }
    }
  }

  /// Output APO -> GUI telemetry (independent of the mic APO running).
  pub fn copy_output_telemetry_from(&self, src: &ShmHeader) {
    let r = Ordering::Relaxed;
    self.out_apo_blocks.store(src.out_apo_blocks.load(r), r);
    self.out_apo_sample_rate.store(src.out_apo_sample_rate.load(r), r);
    self.out_apo_channels.store(src.out_apo_channels.load(r), r);
  }

  /// Publish the combined output EQ. Readers pick it up when `out_eq_version` changes.
  pub fn write_out_eq(&self, bands: &[BiquadBand], preamp_db: f32) {
    let count = write_bands(&self.out_eq_bands, bands);
    self.out_eq_band_count.store(count as u32, Ordering::Relaxed);
    self.out_preamp_db_bits.store(preamp_db.to_bits(), Ordering::Relaxed);
    self.out_eq_version.fetch_add(1, Ordering::Release);
  }

  /// Output EQ bands + preamp (dB). A non-finite preamp reads as 0 dB.
  pub fn read_out_eq(&self) -> (Vec<BiquadBand>, f32) {
    let count = (self.out_eq_band_count.load(Ordering::Acquire) as usize).min(MAX_OUT_EQ_BANDS);
    let preamp = f32::from_bits(self.out_preamp_db_bits.load(Ordering::Relaxed));
    let preamp = if preamp.is_finite() { preamp.clamp(-48.0, 24.0) } else { 0.0 };
    (read_bands(&self.out_eq_bands, count), preamp)
  }

  /// True when the stream APO changes some chat / media stream (not bypassed, a bus not flat).
  /// False = every stream passes through, so pairing streams with apps is pointless.
  pub fn stream_eq_active(&self) -> bool {
    !self.master_bypass.load(Ordering::Relaxed)
      && self.stream_eq.iter().any(|eq| {
        let (bands, preamp) = eq.read();
        !bands.is_empty() || preamp != 0.0
      })
  }

  pub fn write_active_game(&self, game: Option<&ActiveGame>) {
    let empty = ActiveGame::default();
    let game = game.unwrap_or(&empty);
    seq_write(&self.active_game_seq, || {
      write_text(&self.active_game_profile, &game.profile);
      write_text(&self.active_game_label, &game.label);
    });
  }

  /// None = no game active. Retries while a writer is mid-update.
  pub fn read_active_game(&self) -> Option<ActiveGame> {
    for _ in 0..16 {
      let s1 = self.active_game_seq.load(Ordering::Acquire);
      if s1 & 1 == 1 {
        std::hint::spin_loop();
        continue;
      }
      let game =
        ActiveGame { profile: read_text(&self.active_game_profile), label: read_text(&self.active_game_label) };
      // The text loads are relaxed: keep them before the second seq load (ARM64).
      fence(Ordering::Acquire);
      if self.active_game_seq.load(Ordering::Relaxed) == s1 {
        return (!game.label.is_empty()).then_some(game);
      }
    }
    None
  }

  /// DSP -> GUI telemetry, for mirroring the APO's mapping into the daemon's.
  pub fn copy_dsp_telemetry_from(&self, src: &ShmHeader) {
    let r = Ordering::Relaxed;
    self.erle_db_times_10.store(src.erle_db_times_10.load(r), r);
    self.denoise_model_active.store(src.denoise_model_active.load(r), r);
    self.denoise_limit.store(src.denoise_limit.load(r), r);
    self.mic_dsp_load_peak_permille.store(src.mic_dsp_load_peak_permille.load(r), r);
    self.mic_dsp_load_avg_permille.store(src.mic_dsp_load_avg_permille.load(r), r);
    self.mic_dsp_overruns.store(src.mic_dsp_overruns.load(r), r);
    self.mic_leveler_gain_db_x10.store(src.mic_leveler_gain_db_x10.load(r), r);
    self.mic_leveler_level_db_x10.store(src.mic_leveler_level_db_x10.load(r), r);
    self.mic_input_clips.store(src.mic_input_clips.load(r), r);
    self.mic_speech_ms.store(src.mic_speech_ms.load(r), r);
    self.mic_call_jitter_peak_us.store(src.mic_call_jitter_peak_us.load(r), r);
    self.mic_call_gaps.store(src.mic_call_gaps.load(r), r);
    self.aec_state.store(src.aec_state.load(r), r);
    self.aec_delay_us.store(src.aec_delay_us.load(r), r);
    self.aec_raw_delay_us.store(src.aec_raw_delay_us.load(r), r);
    self.aec_confidence_permille.store(src.aec_confidence_permille.load(r), r);
    self.aec_estimate_count.store(src.aec_estimate_count.load(r), r);
    self.aec_delay_from_cache.store(src.aec_delay_from_cache.load(r), r);
    self.aec_double_talk.store(src.aec_double_talk.load(r), r);
    self.aec_ref_level_db_x10.store(src.aec_ref_level_db_x10.load(r), r);
    self.aec_mic_level_db_x10.store(src.aec_mic_level_db_x10.load(r), r);
    self.aec_out_level_db_x10.store(src.aec_out_level_db_x10.load(r), r);
    self.aec_filter_taps.store(src.aec_filter_taps.load(r), r);
    self.aec_pre_delay_us.store(src.aec_pre_delay_us.load(r), r);
    self.aec_corr_max_delay_us.store(src.aec_corr_max_delay_us.load(r), r);
    for (d, s) in self.aec_impulse.iter().zip(&src.aec_impulse) {
      d.store(s.load(r), r);
    }
    for (d, s) in self.aec_corr.iter().zip(&src.aec_corr) {
      d.store(s.load(r), r);
    }
    self.aec_telemetry_seq.fetch_add(1, Ordering::Release);
  }

  /// Publish the microphone EQ. Readers pick it up when `mic_eq_version` changes.
  pub fn write_mic_eq(&self, bands: &[BiquadBand]) {
    let count = write_bands(&self.mic_eq_bands, bands);
    self.mic_eq_band_count.store(count as u32, Ordering::Relaxed);
    self.mic_eq_version.fetch_add(1, Ordering::Release);
  }

  /// Read the microphone EQ. Bands with an unknown filter type are skipped.
  pub fn read_mic_eq(&self) -> Vec<BiquadBand> {
    let count = (self.mic_eq_band_count.load(Ordering::Acquire) as usize).min(MAX_EQ_BANDS);
    read_bands(&self.mic_eq_bands, count)
  }

  /// Ring index of the reference frame that played at `qpc_hns`, or None if no
  /// reference was captured yet. May point into the future (reads then yield silence).
  pub fn ref_index_at(&self, qpc_hns: u64) -> Option<u32> {
    for _ in 0..8 {
      let s1 = self.ref_seq.load(Ordering::Acquire);
      if s1 & 1 == 1 {
        std::hint::spin_loop();
        continue;
      }
      let idx = self.ref_snap_index.load(Ordering::Acquire);
      let t = self.ref_snap_qpc_hns.load(Ordering::Acquire);
      let s2 = self.ref_seq.load(Ordering::Acquire);
      if s1 != s2 {
        continue;
      }
      if t == 0 {
        return None;
      }
      let diff_hns = qpc_hns as i64 - t as i64;
      let frames = diff_hns.saturating_mul(REF_SAMPLE_RATE as i64) / HNS_PER_SEC;
      let frames = frames.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
      return Some(idx.wrapping_add_signed(frames));
    }
    None
  }

  /// Age of the newest reference frame relative to `now_hns`, in 100 ns units.
  pub fn ref_age_hns(&self, now_hns: u64) -> Option<u64> {
    let t = self.ref_snap_qpc_hns.load(Ordering::Acquire);
    (t != 0).then(|| now_hns.saturating_sub(t))
  }

  pub fn read_aec_impulse(&self) -> Vec<f32> {
    self.aec_impulse.iter().map(|a| f32::from_bits(a.load(Ordering::Relaxed))).collect()
  }

  pub fn read_aec_corr(&self) -> Vec<f32> {
    self.aec_corr.iter().map(|a| f32::from_bits(a.load(Ordering::Relaxed))).collect()
  }
}

impl Default for ShmStreamEq {
  fn default() -> Self {
    Self {
      version: AtomicU32::new(0),
      band_count: AtomicU32::new(0),
      preamp_db_bits: AtomicU32::new(0),
      bands: std::array::from_fn(|_| ShmEqBand::default()),
    }
  }
}

impl OwnerWatch {
  /// An owner is publishing settings right now. False until the beat is seen to change, so a
  /// value left behind by an owner that is gone never counts.
  pub fn live(&mut self, h: &ShmHeader, now: Instant) -> bool {
    let beat = h.owner_beat.load(Ordering::Acquire);
    let changed_at = match self.seen {
      None => None,
      Some((last, at)) if last == beat => at,
      Some(_) => Some(now),
    };
    self.seen = Some((beat, changed_at));
    changed_at.is_some_and(|t| now.duration_since(t) < OWNER_TIMEOUT)
  }
}

impl ShmStreamEq {
  /// Publish; the APO picks it up when `version` changes. Empty + 0 dB = pass-through.
  pub fn write(&self, bands: &[BiquadBand], preamp_db: f32) {
    let count = write_bands(&self.bands, bands);
    self.band_count.store(count as u32, Ordering::Relaxed);
    self.preamp_db_bits.store(preamp_db.to_bits(), Ordering::Relaxed);
    self.version.fetch_add(1, Ordering::Release);
  }

  /// Bands + preamp (dB); a non-finite preamp reads as 0 dB.
  pub fn read(&self) -> (Vec<BiquadBand>, f32) {
    let count = (self.band_count.load(Ordering::Acquire) as usize).min(MAX_STREAM_EQ_BANDS);
    let preamp = f32::from_bits(self.preamp_db_bits.load(Ordering::Relaxed));
    let preamp = if preamp.is_finite() { preamp.clamp(-48.0, 24.0) } else { 0.0 };
    (read_bands(&self.bands, count), preamp)
  }
}

/// Returns how many bands were stored (bounded by the slot count).
fn write_bands(slots: &[ShmEqBand], bands: &[BiquadBand]) -> usize {
  // The engine runs plain biquads: a steep cut takes several slots.
  let bands = expand_slopes(bands);
  let count = bands.len().min(slots.len());
  for (slot, band) in slots.iter().zip(bands.iter().take(count)) {
    slot.enabled.store(band.enabled as u32, Ordering::Relaxed);
    slot.filter_type.store(band.filter_type.to_u32(), Ordering::Relaxed);
    slot.frequency_bits.store(band.frequency.to_bits(), Ordering::Relaxed);
    slot.gain_bits.store(band.gain_db.to_bits(), Ordering::Relaxed);
    slot.q_bits.store(band.q.to_bits(), Ordering::Relaxed);
  }
  count
}

/// Bands with an unknown filter type or non-finite values are skipped; the rest clamped.
fn read_bands(slots: &[ShmEqBand], count: usize) -> Vec<BiquadBand> {
  slots
    .iter()
    .take(count)
    .filter_map(|slot| {
      let filter_type = FilterType::from_u32(slot.filter_type.load(Ordering::Relaxed))?;
      let mut band = BiquadBand {
        enabled: slot.enabled.load(Ordering::Relaxed) != 0,
        filter_type,
        frequency: f32::from_bits(slot.frequency_bits.load(Ordering::Relaxed)),
        gain_db: f32::from_bits(slot.gain_bits.load(Ordering::Relaxed)),
        q: f32::from_bits(slot.q_bits.load(Ordering::Relaxed)),
        slope_db: 12,
      };
      if !(band.frequency.is_finite() && band.gain_db.is_finite() && band.q.is_finite()) {
        return None;
      }
      band.clamp_parameters();
      Some(band)
    })
    .collect()
}

/// NUL-terminated UTF-16, truncated to the slot count minus one.
/// Seqlock write (one writer): the counter is odd while `write` runs. It starts from the next
/// odd value whatever the counter holds, so a writer that died between the two steps (process
/// killed, `panic = "abort"`) does not leave it odd for the life of the mapping, where every
/// reader would give up forever.
fn seq_write(seq: &AtomicU32, write: impl FnOnce()) {
  let odd = seq.load(Ordering::Relaxed) | 1;
  seq.store(odd, Ordering::Relaxed);
  fence(Ordering::Release);
  write();
  seq.store(odd.wrapping_add(1), Ordering::Release);
}

fn write_text(slots: &[AtomicU16], text: &str) {
  let room = slots.len().saturating_sub(1);
  let units = text.encode_utf16().take(room).chain(std::iter::repeat(0));
  for (slot, unit) in slots.iter().zip(units) {
    slot.store(unit, Ordering::Relaxed);
  }
}

fn read_text(slots: &[AtomicU16]) -> String {
  let units: Vec<u16> = slots.iter().map(|s| s.load(Ordering::Relaxed)).take_while(|&u| u != 0).collect();
  String::from_utf16_lossy(&units)
}

impl ShmLayout {
  pub fn total_size_bytes() -> usize {
    std::mem::size_of::<ShmLayout>()
  }
}

impl<'a> ShmRingBuffer<'a> {
  /// Safety: caller must guarantee `layout` points to valid initialized ShmLayout memory.
  pub unsafe fn from_raw(layout: &'a mut ShmLayout) -> Self {
    Self { layout }
  }

  /// Write stereo samples from speaker output tap
  pub fn push_frames(&mut self, frames: &[StereoFrame]) {
    let mut write_pos = self.layout.header.write_frame_index.load(Ordering::Relaxed);
    for &frame in frames {
      self.write_slot(write_pos, frame);
      write_pos = write_pos.wrapping_add(1);
    }
    self.layout.header.write_frame_index.store(write_pos, Ordering::Release);
    self.layout.header.tap_active.store(true, Ordering::Relaxed);
  }

  /// Write a timestamped loopback packet (48 kHz). `qpc_first_hns` is the QPC time of
  /// `frames[0]`. Gaps (WASAPI loopback delivers nothing while nothing plays) are filled
  /// with silence so ring index stays linear in time.
  pub fn push_frames_timed(&mut self, frames: &[StereoFrame], qpc_first_hns: u64) {
    let header = &self.layout.header;
    let mut write_pos = header.write_frame_index.load(Ordering::Relaxed);
    let snap_t = header.ref_snap_qpc_hns.load(Ordering::Relaxed);

    if snap_t != 0 {
      // Where the writer should be for this packet's timestamp.
      let expected = header.ref_index_at(qpc_first_hns).unwrap_or(write_pos);
      let gap = expected.wrapping_sub(write_pos) as i32;
      if gap > 0 {
        let fill = (gap as usize).min(RING_BUFFER_CAPACITY);
        for _ in 0..fill {
          self.write_slot(write_pos, StereoFrame::default());
          write_pos = write_pos.wrapping_add(1);
        }
        // Huge gap: jump straight to the expected index (ring is all silence anyway).
        write_pos = expected;
      }
    }

    for &frame in frames {
      self.write_slot(write_pos, frame);
      write_pos = write_pos.wrapping_add(1);
    }

    let end_t = qpc_first_hns + (frames.len() as u64 * HNS_PER_SEC as u64) / REF_SAMPLE_RATE as u64;
    let header = &self.layout.header;
    seq_write(&header.ref_seq, || {
      header.write_frame_index.store(write_pos, Ordering::Release);
      header.ref_snap_index.store(write_pos, Ordering::Release);
      header.ref_snap_qpc_hns.store(end_t, Ordering::Release);
    });
  }

  /// Read mono reference frames starting at ring index `start`. Frames that are not in
  /// the ring (overwritten, or not yet written) read as silence.
  pub fn read_frames_from(&self, start: u32, out: &mut [f32]) {
    let write_pos = self.layout.header.write_frame_index.load(Ordering::Acquire);
    let cap = RING_BUFFER_CAPACITY as u32;
    for (i, dst) in out.iter_mut().enumerate() {
      let pos = start.wrapping_add(i as u32);
      let age = write_pos.wrapping_sub(pos); // 1 = newest frame
      *dst = if age >= 1 && age < cap - 1024 { self.read_slot(pos) } else { 0.0 };
    }
    self.layout.header.mic_active.store(true, Ordering::Relaxed);
  }

  /// Read speaker reference frames for AEC processing
  pub fn read_recent_frames(&self, count: usize, delay_frames: usize, out: &mut [f32]) {
    let write_pos = self.layout.header.write_frame_index.load(Ordering::Acquire);
    let start = write_pos.wrapping_sub((count + delay_frames) as u32);
    let n = count.min(out.len());
    if let Some(dst) = out.get_mut(..n) {
      for (i, d) in dst.iter_mut().enumerate() {
        *d = self.read_slot(start.wrapping_add(i as u32));
      }
    }
    self.layout.header.mic_active.store(true, Ordering::Relaxed);
  }

  pub fn header(&self) -> &ShmHeader {
    &self.layout.header
  }

  pub fn header_mut(&mut self) -> &mut ShmHeader {
    &mut self.layout.header
  }

  fn write_slot(&mut self, pos: u32, frame: StereoFrame) {
    let idx = pos as usize % RING_BUFFER_CAPACITY;
    if let Some(slot) = self.layout.buffer.get_mut(idx) {
      *slot = frame;
    }
  }

  fn read_slot(&self, pos: u32) -> f32 {
    let idx = pos as usize % RING_BUFFER_CAPACITY;
    // Convert stereo to mono reference: (left + right) * 0.5, non-finite = silence.
    let mono = self.layout.buffer.get(idx).map_or(0.0, |f| (f.left + f.right) * 0.5);
    if mono.is_finite() {
      mono.clamp(-REF_SAMPLE_MAX, REF_SAMPLE_MAX)
    } else {
      0.0
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn new_layout() -> Box<ShmLayout> {
    // Built on the heap: the 1 MB buffer would overflow the test thread's stack.
    let mut mem: Box<std::mem::MaybeUninit<ShmLayout>> = Box::new_uninit();
    // SAFETY: all-zero bytes are a valid ShmLayout (atomics, ints, floats); header is
    // then overwritten with defaults.
    unsafe {
      std::ptr::write_bytes(mem.as_mut_ptr(), 0, 1);
      let mut mem = mem.assume_init();
      std::ptr::write(&mut mem.header, ShmHeader::default());
      mem
    }
  }

  #[test]
  fn test_ring_buffer_push_read() {
    let mut mem = new_layout();
    let mut ring = unsafe { ShmRingBuffer::from_raw(&mut mem) };

    let frames = vec![
      StereoFrame { left: 0.2, right: 0.4 },
      StereoFrame { left: 0.6, right: 0.8 },
      StereoFrame { left: 1.0, right: 1.0 },
    ];
    ring.push_frames(&frames);

    let mut out = [0.0; 3];
    ring.read_recent_frames(3, 0, &mut out);

    // mono = (l + r) * 0.5
    assert!((out[0] - 0.3).abs() < 1e-6);
    assert!((out[1] - 0.7).abs() < 1e-6);
    assert!((out[2] - 1.0).abs() < 1e-6);
  }

  #[test]
  fn test_timed_push_maps_time_to_index_and_fills_gaps() {
    let mut mem = new_layout();
    let mut ring = unsafe { ShmRingBuffer::from_raw(&mut mem) };
    let one = [StereoFrame { left: 1.0, right: 1.0 }; 480];

    // Packet at t = 1 s, 10 ms long.
    ring.push_frames_timed(&one, HNS_PER_SEC as u64);
    let idx_start = ring.header().ref_index_at(HNS_PER_SEC as u64);
    assert_eq!(idx_start, Some(0));

    // Next packet 100 ms later (90 ms of nothing played in between).
    ring.push_frames_timed(&one, HNS_PER_SEC as u64 + 1_000_000);
    let idx_second = ring.header().ref_index_at(HNS_PER_SEC as u64 + 1_000_000);
    assert_eq!(idx_second, Some(4800), "100 ms = 4800 frames after the first packet");

    let mut out = [0.0f32; 4];
    ring.read_frames_from(1000, &mut out);
    assert!(out.iter().all(|&s| s == 0.0), "gap must read as silence");
    ring.read_frames_from(4800, &mut out);
    assert!(out.iter().all(|&s| s == 1.0));
    // Future index reads as silence.
    ring.read_frames_from(10_000, &mut out);
    assert!(out.iter().all(|&s| s == 0.0));
  }

  #[test]
  fn test_ring_read_sanitizes_garbage() {
    let mut mem = new_layout();
    let mut ring = unsafe { ShmRingBuffer::from_raw(&mut mem) };
    let frames = [
      StereoFrame { left: f32::INFINITY, right: 0.0 },
      StereoFrame { left: f32::NAN, right: 0.5 },
      StereoFrame { left: 1e30, right: 1e30 },
      StereoFrame { left: 0.5, right: 0.5 },
    ];
    ring.push_frames(&frames);
    let mut out = [1.0f32; 4];
    ring.read_frames_from(0, &mut out);
    assert_eq!(out, [0.0, 0.0, REF_SAMPLE_MAX, 0.5]);
  }

  #[test]
  fn test_mic_eq_roundtrip() {
    let header = ShmHeader::default();
    let bands = vec![
      BiquadBand { filter_type: FilterType::HighPass, frequency: 80.0, ..BiquadBand::default() },
      BiquadBand { enabled: false, gain_db: -3.0, q: 2.0, ..BiquadBand::default() },
    ];
    let v0 = header.mic_eq_version.load(Ordering::Relaxed);
    header.write_mic_eq(&bands);
    assert_ne!(header.mic_eq_version.load(Ordering::Relaxed), v0);
    assert_eq!(header.read_mic_eq(), bands);
  }

  #[test]
  fn test_out_eq_roundtrip_and_mirroring() {
    let src = ShmHeader::default();
    let bands: Vec<BiquadBand> = (0..20)
      .map(|i| BiquadBand { frequency: 100.0 + i as f32 * 100.0, gain_db: 1.0, ..BiquadBand::default() })
      .collect();
    src.write_out_eq(&bands, -3.5);
    src.master_bypass.store(true, Ordering::Relaxed);
    assert_eq!(src.read_out_eq(), (bands.clone(), -3.5));

    let dst = ShmHeader::default();
    dst.copy_controls_from(&src, &mut CopiedEqVersions::default());
    assert_eq!(dst.read_out_eq(), (bands, -3.5));
    assert!(dst.master_bypass.load(Ordering::Relaxed));
  }

  #[test]
  fn test_stream_eq_roundtrip_and_mirroring() {
    let (src, dst) = (ShmHeader::default(), ShmHeader::default());
    let band = BiquadBand { gain_db: -4.0, frequency: 2500.0, ..BiquadBand::default() };
    src.stream_eq[1].write(std::slice::from_ref(&band), -1.5);
    let mut seen = CopiedEqVersions::default();
    dst.copy_controls_from(&src, &mut seen);
    assert_eq!(dst.stream_eq[1].read(), (vec![band], -1.5));
    assert_eq!(dst.stream_eq[0].read(), (vec![], 0.0));
    // Unchanged source: nothing is written again (the APO keeps its coefficients).
    let v = dst.stream_eq[1].version.load(Ordering::Relaxed);
    dst.copy_controls_from(&src, &mut seen);
    assert_eq!(dst.stream_eq[1].version.load(Ordering::Relaxed), v);
  }

  #[test]
  fn owner_watch_needs_a_changing_beat() {
    let h = ShmHeader::default();
    let mut watch = OwnerWatch::default();
    let t0 = Instant::now();
    h.owner_beat.store(41, Ordering::Relaxed);
    assert!(!watch.live(&h, t0), "a leftover beat is not an owner");
    assert!(!watch.live(&h, t0 + Duration::from_millis(100)));
    h.owner_beat.fetch_add(1, Ordering::Relaxed);
    assert!(watch.live(&h, t0 + Duration::from_millis(200)));
    assert!(watch.live(&h, t0 + Duration::from_millis(200) + OWNER_TIMEOUT / 2));
    assert!(!watch.live(&h, t0 + Duration::from_millis(200) + OWNER_TIMEOUT), "owner gone");
    h.owner_beat.fetch_add(1, Ordering::Relaxed);
    assert!(watch.live(&h, t0 + OWNER_TIMEOUT * 2), "owner back");
  }

  #[test]
  fn new_owner_copies_eqs_even_with_equal_versions() {
    let (src, dst) = (ShmHeader::default(), ShmHeader::default());
    let band = BiquadBand { gain_db: 3.0, ..BiquadBand::default() };
    src.write_mic_eq(std::slice::from_ref(&band));
    dst.write_mic_eq(&[]); // another daemon's EQ, same version number
    let apo_cached = dst.mic_eq_version.load(Ordering::Relaxed);
    dst.copy_controls_from(&src, &mut CopiedEqVersions::default());
    assert_eq!(dst.read_mic_eq(), vec![band]);
    // The APO re-reads only on a version it has not seen: it must change.
    assert_ne!(dst.mic_eq_version.load(Ordering::Relaxed), apo_cached);
  }

  #[test]
  fn stream_eq_is_active_only_with_a_non_flat_bus_and_no_bypass() {
    let h = ShmHeader::default();
    assert!(!h.stream_eq_active());
    h.stream_eq[0].write(&[], -2.0);
    assert!(h.stream_eq_active(), "preamp alone changes the stream");
    h.master_bypass.store(true, Ordering::Relaxed);
    assert!(!h.stream_eq_active());
    h.master_bypass.store(false, Ordering::Relaxed);
    h.stream_eq[0].write(&[], 0.0);
    h.stream_eq[1].write(&[BiquadBand::default()], 0.0);
    assert!(h.stream_eq_active());
  }

  #[test]
  fn a_seqlock_left_odd_by_a_dead_writer_recovers_on_the_next_write() {
    let h = ShmHeader::default();
    h.active_game_seq.store(7, Ordering::Relaxed); // writer killed mid-update
    assert_eq!(h.read_active_game(), None);
    let game = ActiveGame { profile: "p".into(), label: "l".into() };
    h.write_active_game(Some(&game));
    assert_eq!(h.read_active_game(), Some(game));
    assert_eq!(h.active_game_seq.load(Ordering::Relaxed) & 1, 0);
  }

  #[test]
  fn test_active_game_text_roundtrip() {
    let h = ShmHeader::default();
    assert_eq!(h.read_active_game(), None);
    let game =
      ActiveGame { profile: "The Division 2".into(), label: "Tom Clancy's The Division 2 (division2.exe)".into() };
    h.write_active_game(Some(&game));
    assert_eq!(h.read_active_game(), Some(game));
    let long = ActiveGame { profile: "\u{e9}".repeat(500), label: "x".into() };
    h.write_active_game(Some(&long));
    assert_eq!(h.read_active_game().map(|g| g.profile.chars().count()), Some(GAME_TEXT_LEN - 1));
    h.write_active_game(None);
    assert_eq!(h.read_active_game(), None);
  }

  #[test]
  fn test_state_enums_roundtrip_and_unknown() {
    for s in
      [AecState::Disabled, AecState::NoReference, AecState::ReferenceSilent, AecState::Measuring, AecState::Locked]
    {
      assert_eq!(AecState::from_u32(s.to_u32()), s);
    }
    assert_eq!(AecState::from_u32(77), AecState::Unknown(77));
    assert_eq!(DenoiseModel::from_u32(1), DenoiseModel::DeepFilterNet);
    assert_eq!(DenoiseModel::from_u32(2), DenoiseModel::DpdfNet);
    for m in [DenoiseModel::RnNoise, DenoiseModel::DeepFilterNet, DenoiseModel::DpdfNet] {
      assert_eq!(DenoiseModel::from_u32(m.to_u32()), m);
    }
    assert_eq!(DenoiseModel::from_u32(9), DenoiseModel::Unknown(9));
    for l in [DenoiseLimit::None, DenoiseLimit::LoadFailed, DenoiseLimit::Overloaded] {
      assert_eq!(DenoiseLimit::from_u32(l.to_u32()), l);
    }
    assert_eq!(DenoiseLimit::from_u32(42), DenoiseLimit::Unknown(42));
  }
}
