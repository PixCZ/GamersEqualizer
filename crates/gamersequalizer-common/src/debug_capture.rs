//! Debug capture: the mic chain (inside the APO) records every stage plus its variables for up
//! to a minute into one Global mapping; a tool (`examples/debug_capture.rs` in the APO crate)
//! requests the capture and writes WAV + TSV files. Only APO builds with the `debug-capture`
//! feature create the mapping, so release builds carry no cost.
//!
//! Protocol (`state`): the tool sets `Requested` (with `target_frames`); the first mic stream
//! to see it swaps it to `Recording`, writes stage audio + one variable row per block, and sets
//! `Done` when `frames` reaches the target. The tool reads after `Done` (Acquire on `state`).

use std::sync::atomic::{AtomicU32, AtomicU64};

/// Bump the version in the name on every layout change.
pub const DEBUG_CAPTURE_NAME: &str = "Global\\GamersEqualizer_DebugCapture_v2";
pub const DEBUG_CAPTURE_MAGIC: u32 = 0x4744_4247; // 'GDBG'
pub const DEBUG_CAPTURE_RATE: u32 = 48_000;
pub const DEBUG_CAPTURE_MAX_SECONDS: u32 = 60;
pub const DEBUG_CAPTURE_MAX_FRAMES: usize = (DEBUG_CAPTURE_RATE * DEBUG_CAPTURE_MAX_SECONDS) as usize;
/// One row per processed block; 60 s of 96-frame (2 ms) blocks. Larger blocks use fewer rows.
pub const DEBUG_CAPTURE_MAX_BLOCKS: usize = DEBUG_CAPTURE_MAX_FRAMES / 96;

pub const STATE_IDLE: u32 = 0;
pub const STATE_REQUESTED: u32 = 1;
pub const STATE_RECORDING: u32 = 2;
pub const STATE_DONE: u32 = 3;

/// (file name, meaning). Audio recorded per block, mono 48 kHz, all sample-aligned (index = same block position).
pub const DEBUG_STAGES: [(&str, &str); 8] = [
  ("01_mic_raw", "chain input (after the APO's resampler, first channel)"),
  ("02_ref_at_mic", "speaker reference that played at the mic block's time, no echo delay"),
  ("03_ref_aec", "reference as fed to the AEC (shifted by delay - pre-delay); 0 if not run"),
  ("04_after_aec", "= raw when AEC is off or not locked"),
  ("05_after_denoise", "after the neural model + gate (delayed by its latency)"),
  ("06_after_eq", "after the parametric voice EQ"),
  ("07_after_leveler", "after the learning voice leveler"),
  ("08_out", "after volume + soft limiter = what leaves the APO"),
];
pub const STAGE_MIC_RAW: usize = 0;
pub const STAGE_REF_AT_MIC: usize = 1;
pub const STAGE_REF_AEC: usize = 2;
pub const STAGE_AFTER_AEC: usize = 3;
pub const STAGE_AFTER_DENOISE: usize = 4;
pub const STAGE_AFTER_EQ: usize = 5;
pub const STAGE_AFTER_LEVELER: usize = 6;
pub const STAGE_OUT: usize = 7;

/// (column, meaning) of the per-block variable table (`vars.tsv`), all stored as f32.
pub const DEBUG_VARS: [(&str, &str); 55] = [
  ("frame", "first frame of the block in the capture"),
  ("frames", "block length"),
  ("t_ms", "block time (mic QPC) since capture start; -1 without a timestamp"),
  ("aec_enabled", "echo cancellation switch (1 on)"),
  ("aec_state", "0 off, 1 no ref, 2 ref silent, 3 measuring, 4 locked"),
  ("delay_samples", "locked echo delay, -1 none"),
  ("delay_from_cache", "1 = delay is the remembered warm start, not measured yet"),
  ("raw_delay_samples", "last window's estimate, -1 none"),
  ("peak_ratio", "last window's correlation peak/average"),
  ("estimate_count", "delay windows that produced an estimate"),
  ("ref_offset_samples", "shift used for 03_ref_aec (delay - pre-delay), -1 not run"),
  ("ref_age_ms", "age of the newest reference frame, -1 none"),
  ("ref_rms_db", "reference fed to the AEC, smoothed, dBFS"),
  ("mic_rms_db", "raw mic, smoothed, dBFS"),
  ("out_rms_db", "chain output, smoothed, dBFS"),
  ("erle_db", "echo return loss enhancement (smoothed, only while far end plays)"),
  ("double_talk", "1 = double-talk detector froze adaptation"),
  ("aec_taps", "adaptive filter length (samples)"),
  ("denoise_selected", "0 RNNoise, 1 DeepFilterNet, 2 DPDFNet"),
  ("denoise_active", "model actually running (fallbacks count)"),
  ("denoise_limit", "0 none, 1 load failed, 2 overloaded"),
  ("ai_strength", "denoise strength 0..1"),
  ("gate_db", "noise gate threshold setting (special values = off / auto)"),
  ("heard_voice", "input voice detector heard speech in this block"),
  ("denoise_lead_samples", "model latency + gate lookahead"),
  ("eq_version", "mic EQ settings version, -1 none"),
  ("leveler_on", "voice leveler switch"),
  ("leveler_target_db", "speech level the leveler aims for, dBFS"),
  ("leveler_gain_db", "gain the leveler applies"),
  ("leveler_level_db", "learned speech level, NaN until learned"),
  ("input_gain_db", "Windows mic volume as the leveler knows it"),
  ("mic_volume", "1.0 = 100 %"),
  ("boost", "gain applied before the limiter"),
  ("load", "processing time / block duration"),
  ("in_prob", "highest raw-input detector voice probability (in_net or RNNoise's own), -1 no frame done"),
  ("out_prob", "highest output detector (out_net) voice probability, -1 no frame done / gate off"),
  ("rescue_prob", "highest click-rescue detector voice probability, -1 rescue off / no frame done"),
  ("vad_run", "input detector: consecutive frames over its open threshold"),
  ("vad_active_left", "input detector: frames a weaker frame still counts as speech"),
  ("vad_out_active_left", "output detector: frames a weaker frame still counts as speech"),
  ("gate_gain_db", "noise gate gain after the block, dB (-100 floor), NaN gate off"),
  ("gate_hold_left", "noise gate hold still to run (samples), -1 gate off"),
  ("gate_floor_db", "closed-gate attenuation incl. the leveler extra, dB, NaN gate off"),
  ("df_pre_gain_db", "DeepFilterNet input pre-gain, dB (applies only while DFN3 runs)"),
  ("rescue_mix", "click rescue: RNNoise's share of the output 0..1"),
  ("rescue_hold_left", "click rescue hold still to run (samples)"),
  ("asleep", "heavy model asleep for a pause, RNNoise stands in (1)"),
  ("bypassed", "denoiser bypassed (strength 0), audio untouched (1)"),
  ("quiet_samples", "input samples since speech or activity was last seen"),
  ("takeover", "model takeover / crossfade in progress (1)"),
  ("lookahead_now", "gate lookahead in the output now (samples)"),
  ("leveler_speech_ms", "speech the leveler has learned from (ms)"),
  ("input_clips", "clipped raw input samples since the stream started (cumulative)"),
  ("mic_auto_level", "Windows mic volume the automatic input gain set, 1.0 = 100 %, 0 none, NaN no shm"),
  ("limiter_gain_db", "peak limiter gain reduction, dB (0 = none)"),
];

#[repr(C)]
pub struct DebugCaptureHeader {
  pub magic: AtomicU32,
  pub state: AtomicU32,
  /// Requested length (frames, at most `DEBUG_CAPTURE_MAX_FRAMES`), set with `Requested`.
  pub target_frames: AtomicU32,
  /// Frames written so far (Release after each block).
  pub frames: AtomicU32,
  pub blocks: AtomicU32,
  /// Sample rate of the device stream (the chain itself always runs at 48 kHz).
  pub device_rate: AtomicU32,
  /// Process id of the recording audiodg.
  pub writer_pid: AtomicU32,
  /// Chain constants for the report.
  pub aec_pre_delay_samples: AtomicU32,
  pub corr_max_delay_samples: AtomicU32,
  /// QPC time (100 ns) of the first recorded block, 0 without timestamps.
  pub start_qpc_hns: AtomicU64,
}

#[repr(C)]
pub struct DebugCaptureLayout {
  pub header: DebugCaptureHeader,
  pub audio: [[f32; DEBUG_CAPTURE_MAX_FRAMES]; DEBUG_STAGES.len()],
  pub vars: [[f32; DEBUG_VARS.len()]; DEBUG_CAPTURE_MAX_BLOCKS],
}

impl DebugCaptureLayout {
  /// ~99 MB of pagefile-backed commit; physical memory only for pages actually written.
  pub const fn total_size_bytes() -> usize {
    std::mem::size_of::<Self>()
  }
}
