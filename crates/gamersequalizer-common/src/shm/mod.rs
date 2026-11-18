use crate::biquad::{MAX_EQ_BANDS, MAX_OUT_EQ_BANDS, MAX_STREAM_EQ_BANDS};
use crate::profile::{BiquadBand, FilterType, expand_slopes};
use crate::sidetone::{NoiseMode, SidetoneRing, SidetoneSettings};
use crate::streams::{STREAM_EQ_BUSES, StreamTable};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU64, Ordering, fence};
use std::time::{Duration, Instant};

mod eq;
mod header;
mod mirror;
mod owner;
mod reference_ring;

pub use eq::{ShmEqBand, ShmStreamEq};
use header::seq_write;
pub use header::{ActiveGame, ShmHeader};
pub use mirror::CopiedEqVersions;
pub use owner::OwnerWatch;
pub use reference_ring::{ShmRingBuffer, StereoFrame};

/// The layout version: the one number to bump on every layout change (`SHM_LAYOUT_SIZE` fails the
/// build until it is bumped). Both mapping names carry it, so a process still running an older
/// build keeps its own mapping instead of both reading each other's memory with different layouts.
macro_rules! shm_version {
  () => {
    23
  };
}
pub const SHM_VERSION: u32 = shm_version!();
/// Size of `ShmLayout` at `SHM_VERSION`.
const SHM_LAYOUT_SIZE: usize = 1_254_832;
const _: () = assert!(size_of::<ShmLayout>() == SHM_LAYOUT_SIZE, "layout changed: bump shm_version! and the size");
pub const SHM_NAME: &str = concat!("Local\\GamersEqualizer_SpeakerRef_Buffer_v", shm_version!());
/// Mapping created by the capture APO inside audiodg (session 0, so it must be Global; only
/// a service may create Global objects). The daemon opens it when it exists and mirrors
/// reference audio + settings in, telemetry out. Same layout as `SHM_NAME`.
pub const APO_SHM_NAME: &str = concat!("Global\\GamersEqualizer_Apo_v", shm_version!());
pub const SHM_MAGIC: u32 = 0x474D4551; // 'GMEQ'
/// Voice leveler target (`mic_leveler_target_db_x10`) until the user changes it: -24 dBFS.
pub const MIC_LEVELER_DEFAULT_TARGET_DB_X10: i32 = -240;
/// 131072 frames = ~2.7 s at 48 kHz: covers the delay estimator's 1 s window + 400 ms search.
pub const RING_BUFFER_CAPACITY: usize = 131072;
/// Reference samples read from the ring are clamped to +-this (+18 dBFS; a float mix may go
/// above 1.0). The ring is written by another process: one inf/NaN/huge sample would otherwise
/// turn the AEC output into NaN and pin the speaker level meter at +inf forever.
const REF_SAMPLE_MAX: f32 = 8.0;
pub const REF_SAMPLE_RATE: u32 = 48_000;
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
/// `mic_chain_flags` bits.
pub const MIC_CHAIN_ASLEEP: u32 = 1;
pub const MIC_CHAIN_BYPASSED: u32 = 1 << 1;
pub const MIC_CHAIN_TAKEOVER: u32 = 1 << 2;
pub const MIC_CHAIN_LOOKAHEAD: u32 = 1 << 3;
/// More mic chains run in the same audiodg (apps record in different processing modes or from
/// different microphones, each gets its own chain); the telemetry is from one of them only.
pub const MIC_CHAIN_OTHERS: u32 = 1 << 4;
/// The echo canceller runs on both channels of the speaker reference (they differ), not on
/// their mono mix.
pub const MIC_CHAIN_STEREO_REF: u32 = 1 << 5;
/// What the echo canceller is doing right now (shown in the UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AecState {
  Disabled,
  /// No reference source exists (`tap_active` false); `AecRefStatus` says why.
  NoReference,
  /// Reference stream exists but nothing is playing.
  ReferenceSilent,
  /// Playing, delay not yet confirmed.
  Measuring,
  /// Delay confirmed, filter running.
  Locked,
  Unknown(u32),
}

/// Where the echo canceller's speaker reference comes from, or why there is none (daemon -> window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AecRefStatus {
  /// The daemon has not decided yet.
  NotChecked,
  /// Our output effect on the reference output writes it.
  OutputEffect,
  /// No output device to take it from.
  NoOutput,
  /// Our output effect is not installed on the reference output.
  EffectMissing,
  /// Installed, but an older build than the app: it does not write the reference.
  EffectOutdated,
  /// The effect runs, but Windows did not say which device it is on, so it cannot tell it is
  /// the reference output.
  EndpointUnknown,
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

/// Full shared memory segment layout
#[repr(C)]
pub struct ShmLayout {
  pub header: ShmHeader,
  pub buffer: [StereoFrame; RING_BUFFER_CAPACITY],
  /// v7: per-stream table of the render SFX instances (only used in the APO mapping).
  /// All zero in a fresh mapping = all slots free.
  pub streams: StreamTable,
  /// v20: sidetone samples from the mic APO to the output APO (only used in the APO mapping).
  pub sidetone: SidetoneRing,
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

impl AecRefStatus {
  pub fn to_u32(self) -> u32 {
    match self {
      AecRefStatus::NotChecked => 0,
      AecRefStatus::OutputEffect => 1,
      AecRefStatus::NoOutput => 2,
      AecRefStatus::EffectMissing => 3,
      AecRefStatus::EffectOutdated => 4,
      AecRefStatus::EndpointUnknown => 5,
      AecRefStatus::Unknown(v) => v,
    }
  }

  pub fn from_u32(v: u32) -> Self {
    match v {
      0 => AecRefStatus::NotChecked,
      1 => AecRefStatus::OutputEffect,
      2 => AecRefStatus::NoOutput,
      3 => AecRefStatus::EffectMissing,
      4 => AecRefStatus::EffectOutdated,
      5 => AecRefStatus::EndpointUnknown,
      other => AecRefStatus::Unknown(other),
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

impl ShmLayout {
  pub fn total_size_bytes() -> usize {
    std::mem::size_of::<ShmLayout>()
  }
}

#[cfg(test)]
mod tests;
