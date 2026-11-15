//! Turns mic DSP telemetry into log lines: stream started/stopped, model actually running,
//! CPU overload (late blocks) and what was limited because of it, plus a debug summary of the
//! audio engine's call timing (jitter, stalls) and DSP load. The APO runs in audiodg
//! without a logger, so this (in the process that owns the shared memory) logs for it too.

use gamersequalizer_common::shm::{DenoiseLimit, DenoiseModel, ShmHeader};
use gamersequalizer_mic_chain::ai_denoiser::AiVoiceIsolator;
use log::{debug, error, info, warn};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// No new telemetry for this long = no app uses the microphone.
const IDLE_AFTER: Duration = Duration::from_secs(2);
/// Late blocks are summed and reported at most this often (no log flood on a slow PC).
const OVERLOAD_REPORT_EVERY: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct DspWatch {
  seq: Option<u32>,
  last_change: Option<Instant>,
  live: bool,
  active: Option<DenoiseModel>,
  limit: Option<DenoiseLimit>,
  overruns: Option<u32>,
  late_unreported: u32,
  peak_unreported: u32,
  last_report: Option<Instant>,
  /// Debug summary since the last report: worst call jitter (us), stalled calls, DSP load.
  gaps: Option<u32>,
  gaps_unreported: u32,
  jitter_peak_us: u32,
  load_peak: u32,
  load_avg_max: u32,
  /// Last logged measured chain latency (us).
  latency_us: Option<u32>,
}

impl DspWatch {
  /// Mic processing (APO or bridge) produced telemetry within `IDLE_AFTER`.
  pub fn is_live(&self) -> bool {
    self.live
  }

  pub fn check(&mut self, h: &ShmHeader) {
    let now = Instant::now();
    let seq = h.aec_telemetry_seq.load(Ordering::Acquire);
    let changed = self.seq.is_some_and(|s| s != seq);
    if self.seq.is_none() || changed {
      self.seq = Some(seq);
    }
    if changed {
      self.last_change = Some(now);
      if !self.live {
        self.live = true;
        info!("[Mic] microphone processing started (an app opened the microphone)");
      }
    } else if self.live && self.last_change.is_some_and(|t| now.duration_since(t) >= IDLE_AFTER) {
      self.live = false;
      self.active = None;
      self.limit = None;
      self.flush_overload(h);
      self.latency_us = None;
      info!("[Mic] microphone processing stopped (no app uses the microphone)");
    }
    if !changed {
      return;
    }

    let active = DenoiseModel::from_u32(h.denoise_model_active.load(Ordering::Relaxed));
    if self.active != Some(active) {
      self.active = Some(active);
      info!("[Denoise] running: {active:?} (adds {:.0} ms latency)", AiVoiceIsolator::latency_ms(active));
    }
    let limit = DenoiseLimit::from_u32(h.denoise_limit.load(Ordering::Relaxed));
    let selected = DenoiseModel::from_u32(h.denoise_model.load(Ordering::Relaxed));
    if self.limit != Some(limit) {
      self.limit = Some(limit);
      match limit {
        DenoiseLimit::None => {}
        DenoiseLimit::LoadFailed => {
          error!("[Denoise] {selected:?} could not be loaded by the audio engine, RNNoise runs instead")
        }
        DenoiseLimit::Overloaded => warn!(
          "[Denoise] LIMITED: {selected:?} is too slow for real time on this CPU, switched to RNNoise. Select it again to retry."
        ),
        DenoiseLimit::Unknown(v) => warn!("[Denoise] unknown limit code {v}"),
      }
    }

    self.log_latency(h);

    let total = h.mic_dsp_overruns.load(Ordering::Relaxed);
    // A new stream restarts the count at 0: only growth counts.
    let new_late = self.overruns.map_or(0, |prev| total.saturating_sub(prev));
    self.overruns = Some(total);
    if new_late > 0 {
      self.late_unreported += new_late;
      self.peak_unreported = self.peak_unreported.max(h.mic_dsp_load_peak_permille.load(Ordering::Relaxed));
    }
    let gaps = h.mic_call_gaps.load(Ordering::Relaxed);
    self.gaps_unreported += self.gaps.map_or(0, |prev| gaps.saturating_sub(prev));
    self.gaps = Some(gaps);
    self.jitter_peak_us = self.jitter_peak_us.max(h.mic_call_jitter_peak_us.load(Ordering::Relaxed));
    self.load_peak = self.load_peak.max(h.mic_dsp_load_peak_permille.load(Ordering::Relaxed));
    self.load_avg_max = self.load_avg_max.max(h.mic_dsp_load_avg_permille.load(Ordering::Relaxed));
    if self.last_report.is_none_or(|t| now.duration_since(t) >= OVERLOAD_REPORT_EVERY) {
      self.flush_overload(h);
      self.flush_timing();
      self.last_report = Some(now);
    }
  }

  /// Measured chain latency, logged when it first appears and when it moves by 1 ms or more
  /// (a setting changed it), with what each stage holds.
  fn log_latency(&mut self, h: &ShmHeader) {
    let us = h.mic_latency_measured_us.load(Ordering::Relaxed);
    if us == 0 || self.latency_us.is_some_and(|prev| prev.abs_diff(us) < 1000) {
      return;
    }
    self.latency_us = Some(us);
    let ms = |v: u32| v as f32 / 1000.0;
    info!(
      "[Mic] measured latency {:.1} ms (echo cancellation {:.1}, noise suppression {:.1}, word-onset protection {:.1} ms)",
      ms(us),
      ms(h.mic_latency_aec_us.load(Ordering::Relaxed)),
      ms(h.mic_latency_denoise_us.load(Ordering::Relaxed)),
      ms(h.mic_latency_lookahead_us.load(Ordering::Relaxed))
    );
  }

  fn flush_timing(&mut self) {
    debug!(
      "[DSP] mic timing: call jitter peak {:.1} ms, {} stalled calls (>1.5 blocks late), DSP load peak {:.0} %, avg max {:.0} %",
      self.jitter_peak_us as f32 / 1000.0,
      self.gaps_unreported,
      self.load_peak as f32 / 10.0,
      self.load_avg_max as f32 / 10.0
    );
    self.gaps_unreported = 0;
    self.jitter_peak_us = 0;
    self.load_peak = 0;
    self.load_avg_max = 0;
  }

  fn flush_overload(&mut self, h: &ShmHeader) {
    if self.late_unreported == 0 {
      return;
    }
    let active = DenoiseModel::from_u32(h.denoise_model_active.load(Ordering::Relaxed));
    warn!(
      "[DSP] CPU could not keep up: {} microphone audio blocks were late (peak {:.0} % of the time budget, {active:?}). Expect crackles or dropouts.",
      self.late_unreported,
      self.peak_unreported as f32 / 10.0
    );
    self.late_unreported = 0;
    self.peak_unreported = 0;
  }
}
