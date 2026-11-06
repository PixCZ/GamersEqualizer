//! Turns mic DSP telemetry into log lines: stream started/stopped, model actually running,
//! CPU overload (late blocks) and what was limited because of it. The APO runs in audiodg
//! without a logger, so this (in the process that owns the shared memory) logs for it too.

use gamersequalizer_common::shm::{DenoiseLimit, DenoiseModel, ShmHeader};
use gamersequalizer_mic_vst::ai_denoiser::AiVoiceIsolator;
use log::{error, info, warn};
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

    let total = h.mic_dsp_overruns.load(Ordering::Relaxed);
    // A new stream restarts the count at 0: only growth counts.
    let new_late = self.overruns.map_or(0, |prev| total.saturating_sub(prev));
    self.overruns = Some(total);
    if new_late > 0 {
      self.late_unreported += new_late;
      self.peak_unreported = self.peak_unreported.max(h.mic_dsp_load_peak_permille.load(Ordering::Relaxed));
    }
    if self.last_report.is_none_or(|t| now.duration_since(t) >= OVERLOAD_REPORT_EVERY) {
      self.flush_overload(h);
      self.last_report = Some(now);
    }
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
